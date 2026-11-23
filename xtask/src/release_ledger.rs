// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask release-ledger`: check the v0.5.0 release evidence ledger (ADR-4935).
//!
//! The ledger (`docs/release/v0.5.0-evidence-ledger.md`) has one table row per burn-down item:
//! its id, cut class, status, job, lane, branch head commit, integration state, evidence files,
//! platforms proven and what is still open. A row is a claim about other branches and reports, and
//! those move every hour, so this command checks the claims against git and the file system:
//!
//! - every burn-down id ([`EXPECTED_IDS`], 84 rows) appears exactly once and no unknown id does;
//! - `Cut` is `noncuttable`, `cuttable` or `optional`; `Status` is one of [`STATUSES`];
//! - every cited head is a commit in this repository (`git cat-file -e <sha>^{commit}`);
//! - every evidence path exists: `scratch:<rel>` under the scratch root (lane reports live
//!   outside the repository), any other path in the tree **at that row's head**
//!   (`git cat-file -e <head>:<path>`), or in the working tree when the row has no head;
//! - the `Integration` cell is honest: a head that is an ancestor of one of the ledger's
//!   `Integ lines:` refs must name such a ref, a head that is in none must say
//!   `pending integration`, and a row without a head must say `pending integration` too;
//! - a `done` row cites evidence, and an open row (not `done`, or pending integration) says what
//!   is still open; `Job` is a burn-down job id (`J` and two digits);
//! - every `## Supporting lanes` row (restored work with no single burn-down row) names a lane
//!   once, its head is a commit and its report exists;
//! - the `## Open noncuttable rows` list at the top names exactly the noncuttable rows that are
//!   open, so the summary a reader sees first cannot drift from the table.
//!
//! With ancestry on, each row's lane branch (`origin/lane/sprint-<lane>`) is also resolved: a tip
//! that moved past the cited head, or a branch missing on the remote, is a note (the row may need a
//! refresh), and `--strict` turns those notes into failures for the final release-candidate pass.
//!
//! `--no-scratch` skips the scratch paths (hosts without the lead's scratch disk) and
//! `--no-ancestry` skips the integration check (a clone without the integration refs); both are
//! reported, never silently passed. `--print-open` prints the open-noncuttable list the table
//! implies, for pasting after an edit; `--json` prints the whole result as one object
//! ([`to_json`]). The command never edits the ledger and never runs builds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;

/// The ledger, relative to the workspace root.
pub const LEDGER: &str = "docs/release/v0.5.0-evidence-ledger.md";
/// Default scratch root that `scratch:` evidence paths resolve under (the lead's report disk).
pub const DEFAULT_SCRATCH: &str = "/srv/regia-scratch";
/// The status words a row may carry.
pub const STATUSES: [&str; 6] = ["done", "partial", "blocked", "not-done", "in-progress", "not-run"];
/// The cut classes a row may carry (`docs/release/v0.5.0-evidence-ledger.md` § Cut classes).
pub const CUTS: [&str; 3] = ["noncuttable", "cuttable", "optional"];
/// The integration cell of a row whose head is in no integration line.
pub const PENDING: &str = "pending integration";
/// Heading of the summary list at the top of the ledger.
pub const OPEN_HEADING: &str = "## Open noncuttable rows";
/// Heading of the table section.
pub const TABLE_HEADING: &str = "## Ledger";
/// Prefix of a lane's branch on the remote: lane `v05-x` is `origin/lane/sprint-v05-x`.
pub const LANE_REF_PREFIX: &str = "origin/lane/sprint-";
/// Heading of the supporting-lanes table (lanes with no single burn-down row).
pub const SUPPORT_HEADING: &str = "## Supporting lanes";

/// The burn-down item ids, in table order: v0.3.0 (L), v0.3.x m (M), a (A), b (B), v0.4.0 (C),
/// v0.5.0 (N), the v0.5.0 demo (D) and the matrix carry-over (X), as the v0.5 burn-down plan of
/// 2026-11-08 numbers them.
pub const EXPECTED_IDS: [(&str, u32); 8] = [
    ("L", 7),
    ("M", 6),
    ("A", 15),
    ("B", 20),
    ("C", 12),
    ("N", 13),
    ("D", 5),
    ("X", 6),
];

/// Every expected id (`L01` … `X06`) in table order.
pub fn expected_ids() -> Vec<String> {
    EXPECTED_IDS
        .iter()
        .flat_map(|(prefix, count)| (1..=*count).map(move |n| format!("{prefix}{n:02}")))
        .collect()
}

/// Arguments of `cargo xtask release-ledger`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Ledger file, relative to the workspace root.
    #[arg(long, default_value = LEDGER)]
    pub ledger: PathBuf,
    /// Root that `scratch:` evidence paths resolve under.
    #[arg(long, default_value = DEFAULT_SCRATCH)]
    pub scratch: PathBuf,
    /// Skip `scratch:` evidence paths (reported as skipped, not passed).
    #[arg(long)]
    pub no_scratch: bool,
    /// Skip the integration-line ancestry check (reported as skipped).
    #[arg(long)]
    pub no_ancestry: bool,
    /// Print the open-noncuttable list the table implies, then check as usual.
    #[arg(long)]
    pub print_open: bool,
    /// Print the result as one JSON object (`format_version` 1) instead of text lines.
    #[arg(long)]
    pub json: bool,
    /// Fail (instead of noting) when a lane branch moved past its cited head or is missing.
    #[arg(long)]
    pub strict: bool,
    /// Check this checkout instead of the one xtask was built from (ledger path and git
    /// repository both resolve here).
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

/// One table row as the ledger states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 1-based line number in the ledger.
    pub line: usize,
    /// Burn-down id, e.g. `L01`.
    pub id: String,
    /// Short item name.
    pub item: String,
    /// Burn-down job id, e.g. `J05`.
    pub job: String,
    /// Lane name (its branch is [`LANE_REF_PREFIX`] + lane), or `—`.
    pub lane: String,
    /// Cut class (one of [`CUTS`] when valid).
    pub cut: String,
    /// Status word (one of [`STATUSES`] when valid).
    pub status: String,
    /// Head commit, or `None` for `—`.
    pub head: Option<String>,
    /// Integration cell: [`PENDING`] or the backticked ref named.
    pub integration: String,
    /// Evidence paths (backticked items of the cell).
    pub evidence: Vec<String>,
    /// What is still open (free text).
    pub open: String,
}

impl Row {
    /// The row is open: not `done`, or not yet in an integration line.
    pub fn is_open(&self) -> bool {
        self.status != "done" || self.integration == PENDING
    }
}

/// The parsed ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    /// Integration refs named on the `Integ lines:` line.
    pub integ_lines: Vec<String>,
    /// Ids listed under [`OPEN_HEADING`], with their line numbers.
    pub open_listed: Vec<(usize, String)>,
    /// Table rows in file order.
    pub rows: Vec<Row>,
    /// Supporting-lane rows in file order.
    pub supporting: Vec<Support>,
}

/// One `## Supporting lanes` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Support {
    /// 1-based line number in the ledger.
    pub line: usize,
    /// Lane name.
    pub lane: String,
    /// Head commit, or `None` for `—`.
    pub head: Option<String>,
    /// Backticked paths of the `Report` cell.
    pub evidence: Vec<String>,
}

/// What the checker needs from git; the real one shells out, tests use a fake.
pub trait Git {
    /// `rev` names a commit.
    fn is_commit(&self, rev: &str) -> bool;
    /// `path` exists in the tree of commit `rev`.
    fn has_path(&self, rev: &str, path: &str) -> bool;
    /// `Some(true)` when `ancestor` is an ancestor of (or equal to) `of`; `None` when `of` does
    /// not resolve.
    fn is_ancestor(&self, ancestor: &str, of: &str) -> Option<bool>;
    /// The full commit id `rev` names, or `None` when it does not resolve.
    fn resolve(&self, rev: &str) -> Option<String>;
}

/// [`Git`] over the `git` binary in one repository.
#[derive(Debug, Clone)]
pub struct SystemGit {
    /// Repository directory `git -C` runs in.
    pub dir: PathBuf,
}

impl SystemGit {
    fn status(&self, args: &[&str]) -> Option<bool> {
        Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .map(|s| s.success())
    }
}

impl Git for SystemGit {
    fn is_commit(&self, rev: &str) -> bool {
        self.status(&["cat-file", "-e", &format!("{rev}^{{commit}}")]) == Some(true)
    }

    fn has_path(&self, rev: &str, path: &str) -> bool {
        self.status(&["cat-file", "-e", &format!("{rev}:{path}")]) == Some(true)
    }

    fn resolve(&self, rev: &str) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !sha.is_empty()).then_some(sha)
    }

    fn is_ancestor(&self, ancestor: &str, of: &str) -> Option<bool> {
        if !self.is_commit(of) {
            return None;
        }
        self.status(&["merge-base", "--is-ancestor", ancestor, of])
    }
}

/// Backticked items of a cell (`` `a` `b` `` → `["a", "b"]`).
fn backticked(cell: &str) -> Vec<String> {
    cell.split('`')
        .enumerate()
        .filter(|(i, s)| i % 2 == 1 && !s.trim().is_empty())
        .map(|(_, s)| s.trim().to_string())
        .collect()
}

/// The cells of a markdown table line (`| a | b |` → `["a", "b"]`), or `None` if not a row.
fn cells(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !t.starts_with('|') || !t.ends_with('|') || t.len() < 2 {
        return None;
    }
    Some(
        t[1..t.len() - 1]
            .split('|')
            .map(|c| c.trim().to_string())
            .collect(),
    )
}

/// A plain cell value: backticks and bold markers stripped, lower-cased.
fn word(cell: &str) -> String {
    cell.replace(['`', '*'], "").trim().to_lowercase()
}

/// Parse the ledger text. Fails only on a shape problem that makes the table unreadable (no
/// table heading, or a missing column); content problems are [`check`]'s job.
pub fn parse(text: &str) -> Result<Ledger> {
    const COLUMNS: [&str; 10] = [
        "id",
        "cut",
        "status",
        "job",
        "lane",
        "head",
        "integration",
        "evidence",
        "open",
        "item",
    ];
    let mut ledger = Ledger::default();
    let mut section = "";
    let mut header: Option<BTreeMap<String, usize>> = None;
    let mut saw_table = false;
    for (n, line) in text.lines().enumerate() {
        let line_no = n + 1;
        if let Some(rest) = line.trim().strip_prefix("Integ lines:") {
            ledger.integ_lines.extend(backticked(rest));
            continue;
        }
        if line.starts_with("## ") {
            section = if line.trim() == OPEN_HEADING {
                "open"
            } else if line.trim() == TABLE_HEADING {
                saw_table = true;
                "table"
            } else if line.trim() == SUPPORT_HEADING {
                "support"
            } else {
                ""
            };
            header = None;
            continue;
        }
        match section {
            "open" => {
                if let Some(rest) = line.trim_start().strip_prefix("- ") {
                    let first = rest.split_whitespace().next().unwrap_or("");
                    let id = first.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                    if !id.is_empty() {
                        ledger.open_listed.push((line_no, id.to_string()));
                    }
                }
            }
            "support" => {
                let Some(c) = cells(line) else { continue };
                if header.is_none() {
                    let map: BTreeMap<String, usize> =
                        c.iter().enumerate().map(|(i, h)| (word(h), i)).collect();
                    for col in ["lane", "head", "report"] {
                        if !map.contains_key(col) {
                            bail!("ledger line {line_no}: the supporting-lanes header has no `{col}` column");
                        }
                    }
                    header = Some(map);
                    continue;
                }
                if c.iter().all(|x| x.chars().all(|ch| ch == '-' || ch == ':')) {
                    continue;
                }
                let Some(h) = header.as_ref() else { continue };
                let get = |col: &str| h.get(col).and_then(|&i| c.get(i)).cloned().unwrap_or_default();
                ledger.supporting.push(Support {
                    line: line_no,
                    lane: word(&get("lane")),
                    head: backticked(&get("head")).into_iter().next(),
                    evidence: backticked(&get("report")),
                });
            }
            "table" => {
                let Some(c) = cells(line) else { continue };
                if header.is_none() {
                    let map: BTreeMap<String, usize> =
                        c.iter().enumerate().map(|(i, h)| (word(h), i)).collect();
                    for col in COLUMNS {
                        if !map.contains_key(col) {
                            bail!("ledger line {line_no}: the table header has no `{col}` column");
                        }
                    }
                    header = Some(map);
                    continue;
                }
                if c.iter().all(|x| x.chars().all(|ch| ch == '-' || ch == ':')) {
                    continue;
                }
                let Some(h) = header.as_ref() else { continue };
                let get = |col: &str| h.get(col).and_then(|&i| c.get(i)).cloned().unwrap_or_default();
                let head_cell = get("head");
                let head = backticked(&head_cell).into_iter().next();
                let integration_cell = get("integration");
                let integration = if word(&integration_cell) == PENDING {
                    PENDING.to_string()
                } else {
                    backticked(&integration_cell)
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| word(&integration_cell))
                };
                ledger.rows.push(Row {
                    line: line_no,
                    id: word(&get("id")).to_uppercase(),
                    item: get("item"),
                    job: word(&get("job")).to_uppercase(),
                    lane: word(&get("lane")),
                    cut: word(&get("cut")),
                    status: word(&get("status")),
                    head,
                    integration,
                    evidence: backticked(&get("evidence")),
                    open: get("open").trim().trim_matches('—').trim().to_string(),
                });
            }
            _ => {}
        }
    }
    if !saw_table {
        bail!("ledger has no `{TABLE_HEADING}` section");
    }
    Ok(ledger)
}

/// Where `scratch:` paths resolve, or `None` to skip them.
#[derive(Debug, Clone)]
pub struct Roots {
    /// Working tree, for evidence of rows without a head.
    pub worktree: PathBuf,
    /// Scratch root, or `None` with `--no-scratch`.
    pub scratch: Option<PathBuf>,
    /// Check integration-line ancestry and lane-branch tips.
    pub ancestry: bool,
    /// Lane tips that moved or vanished are failures, not notes.
    pub strict: bool,
}

/// The outcome of [`check`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Failures, each prefixed with the ledger line and row id.
    pub problems: Vec<String>,
    /// Checks that were skipped by flag (never counted as passed).
    pub skipped: Vec<String>,
    /// Rows that may need a refresh (a lane branch moved or is missing); failures with `strict`.
    pub notes: Vec<String>,
    /// Noncuttable rows that are open, in table order.
    pub open_noncuttable: Vec<String>,
}

fn is_hex_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_job_id(s: &str) -> bool {
    s.len() == 3 && s.starts_with('J') && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Check one evidence path for the row at `at` with head `head`; `None` when it passes, and
/// `Some(None)` when it was skipped (a scratch path with no scratch root).
fn evidence_problem(
    at: &str,
    ev: &str,
    head: Option<&str>,
    git: &dyn Git,
    roots: &Roots,
) -> Option<Option<String>> {
    if let Some(rel) = ev.strip_prefix("scratch:") {
        let root = match &roots.scratch {
            Some(root) => root,
            None => return Some(None),
        };
        return (!root.join(rel).is_file()).then(|| {
            Some(format!(
                "{at}: evidence `{ev}` not found under {}",
                root.display()
            ))
        });
    }
    if ev.starts_with('/') || ev.contains("..") {
        return Some(Some(format!(
            "{at}: evidence `{ev}` must be repo-relative or `scratch:`"
        )));
    }
    match head.filter(|h| is_hex_sha(h)) {
        Some(head) => (!git.has_path(head, ev))
            .then(|| Some(format!("{at}: evidence `{ev}` is not in the tree of {head}"))),
        None => (!roots.worktree.join(ev).exists())
            .then(|| Some(format!("{at}: evidence `{ev}` not found in the working tree"))),
    }
}

/// Check a parsed ledger against git and the file system (rules in the module docs).
pub fn check(ledger: &Ledger, git: &dyn Git, roots: &Roots) -> Outcome {
    let mut out = Outcome::default();
    let expected = expected_ids();
    let expected_set: BTreeSet<&str> = expected.iter().map(String::as_str).collect();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();

    let mut integ_ok: Vec<&str> = Vec::new();
    if roots.ancestry {
        if ledger.integ_lines.is_empty() {
            out.problems
                .push("ledger names no `Integ lines:` refs".to_string());
        }
        for r in &ledger.integ_lines {
            if git.is_commit(r) {
                integ_ok.push(r);
            } else {
                out.problems.push(format!(
                    "integration line `{r}` does not resolve (fetch it first)"
                ));
            }
        }
    } else {
        out.skipped
            .push("integration ancestry (--no-ancestry)".to_string());
    }
    let mut skipped_scratch = 0usize;

    for row in &ledger.rows {
        let at = format!("line {} {}", row.line, row.id);
        if !expected_set.contains(row.id.as_str()) {
            out.problems.push(format!("{at}: unknown burn-down id"));
        }
        if let Some(prev) = seen.insert(row.id.as_str(), row.line) {
            out.problems
                .push(format!("{at}: duplicate id (first on line {prev})"));
        }
        if !CUTS.contains(&row.cut.as_str()) {
            out.problems
                .push(format!("{at}: cut `{}` is not one of {CUTS:?}", row.cut));
        }
        if !STATUSES.contains(&row.status.as_str()) {
            out.problems.push(format!(
                "{at}: status `{}` is not one of {STATUSES:?}",
                row.status
            ));
        }
        if let Some(head) = &row.head {
            if !is_hex_sha(head) {
                out.problems
                    .push(format!("{at}: head `{head}` is not a 7-40 digit hex commit"));
            } else if !git.is_commit(head) {
                out.problems
                    .push(format!("{at}: head `{head}` is not a commit in this repository"));
            }
        }
        if !is_job_id(&row.job) {
            out.problems.push(format!(
                "{at}: job `{}` is not a burn-down job id (J and two digits)",
                row.job
            ));
        }
        for ev in &row.evidence {
            match evidence_problem(&at, ev, row.head.as_deref(), git, roots) {
                Some(Some(p)) => out.problems.push(p),
                Some(None) => skipped_scratch += 1,
                None => {}
            }
        }
        if row.status == "done" && row.evidence.is_empty() {
            out.problems
                .push(format!("{at}: a `done` row must cite evidence"));
        }
        if roots.ancestry {
            match row.head.as_deref().filter(|h| is_hex_sha(h)) {
                None => {
                    if row.integration != PENDING {
                        out.problems
                            .push(format!("{at}: a row without a head must say `{PENDING}`"));
                    }
                }
                Some(head) => {
                    let containing: Vec<&str> = integ_ok
                        .iter()
                        .copied()
                        .filter(|r| git.is_ancestor(head, r) == Some(true))
                        .collect();
                    if row.integration == PENDING {
                        if let Some(r) = containing.first() {
                            out.problems.push(format!(
                                "{at}: says `{PENDING}` but {head} is in `{r}`; update the row"
                            ));
                        }
                    } else if !ledger.integ_lines.contains(&row.integration) {
                        out.problems.push(format!(
                            "{at}: integration `{}` is neither `{PENDING}` nor a listed integ line",
                            row.integration
                        ));
                    } else if integ_ok.contains(&row.integration.as_str())
                        && !containing.contains(&row.integration.as_str())
                    {
                        out.problems.push(format!(
                            "{at}: {head} is not in `{}`; say `{PENDING}` or name a line that has it",
                            row.integration
                        ));
                    }
                }
            }
        }
        if roots.ancestry
            && let Some(head) = row.head.as_deref().filter(|h| is_hex_sha(h))
            && !row.lane.is_empty()
            && row.lane != "—"
        {
            let branch = format!("{LANE_REF_PREFIX}{}", row.lane);
            // A merged lane branch is deleted after its integration (branch cleanup); when the
            // cited head is in a listed integ line, that is a note even under `--strict`
            // (ADR-5806). A moved branch, or a missing one whose head is in no line, still fails.
            let merged_and_deleted = || {
                integ_ok
                    .iter()
                    .find(|r| git.is_ancestor(head, r) == Some(true))
                    .copied()
            };
            let (note, failure) = match git.resolve(&branch) {
                Some(tip) if tip.starts_with(head) => (None, false),
                Some(tip) => (
                    Some(format!(
                        "{at}: lane {} moved: `{branch}` is at {tip}, the row cites {head}",
                        row.lane
                    )),
                    true,
                ),
                None => match merged_and_deleted() {
                    Some(line) => (
                        Some(format!(
                            "{at}: lane {} has no `{branch}`; its head {head} is in `{line}` (merged, branch deleted)",
                            row.lane
                        )),
                        false,
                    ),
                    None => (
                        Some(format!(
                            "{at}: lane {} has no `{branch}` (push it or cite another ref)",
                            row.lane
                        )),
                        true,
                    ),
                },
            };
            if let Some(n) = note {
                if roots.strict && failure {
                    out.problems.push(n);
                } else {
                    out.notes.push(n);
                }
            }
        }
        if row.is_open() && row.open.is_empty() {
            out.problems
                .push(format!("{at}: an open row must say what is still open"));
        }
        if row.cut == "noncuttable" && row.is_open() {
            out.open_noncuttable.push(row.id.clone());
        }
    }
    let mut lanes: BTreeMap<&str, usize> = BTreeMap::new();
    for sup in &ledger.supporting {
        let at = format!("line {} supporting lane {}", sup.line, sup.lane);
        if let Some(prev) = lanes.insert(sup.lane.as_str(), sup.line) {
            out.problems
                .push(format!("{at}: lane listed twice (first on line {prev})"));
        }
        match sup.head.as_deref() {
            Some(head) if !is_hex_sha(head) => {
                out.problems
                    .push(format!("{at}: head `{head}` is not a 7-40 digit hex commit"));
            }
            Some(head) if !git.is_commit(head) => {
                out.problems
                    .push(format!("{at}: head `{head}` is not a commit in this repository"));
            }
            Some(_) => {}
            None => out
                .problems
                .push(format!("{at}: a supporting lane must cite its head")),
        }
        if sup.evidence.is_empty() {
            out.problems
                .push(format!("{at}: a supporting lane must cite its report"));
        }
        for ev in &sup.evidence {
            match evidence_problem(&at, ev, sup.head.as_deref(), git, roots) {
                Some(Some(p)) => out.problems.push(p),
                Some(None) => skipped_scratch += 1,
                None => {}
            }
        }
    }
    for id in &expected {
        if !seen.contains_key(id.as_str()) {
            out.problems.push(format!("{id}: burn-down id has no ledger row"));
        }
    }
    let listed: BTreeSet<&str> = ledger.open_listed.iter().map(|(_, id)| id.as_str()).collect();
    let implied: BTreeSet<&str> = out.open_noncuttable.iter().map(String::as_str).collect();
    for (line, id) in &ledger.open_listed {
        if !implied.contains(id.as_str()) {
            out.problems.push(format!(
                "line {line} {id}: listed under `{OPEN_HEADING}` but the table row is not an open noncuttable row"
            ));
        }
    }
    for id in implied.difference(&listed) {
        out.problems.push(format!(
            "{id}: open noncuttable row missing from `{OPEN_HEADING}`"
        ));
    }
    if listed.len() != ledger.open_listed.len() {
        out.problems.push(format!("`{OPEN_HEADING}` lists an id twice"));
    }
    if skipped_scratch > 0 {
        out.skipped
            .push(format!("{skipped_scratch} scratch evidence paths (--no-scratch)"));
    }
    out
}

/// Version of the `--json` object; bump it when a key changes meaning or goes away.
pub const JSON_FORMAT_VERSION: u32 = 1;

/// The `--json` object: every row's claims, the open noncuttable ids, problems, notes and skips.
pub fn to_json(ledger: &Ledger, out: &Outcome) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = ledger
        .rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "item": r.item,
                "cut": r.cut,
                "status": r.status,
                "job": r.job,
                "lane": r.lane,
                "head": r.head,
                "integration": r.integration,
                "evidence": r.evidence,
                "open": r.open,
                "is_open": r.is_open(),
            })
        })
        .collect();
    serde_json::json!({
        "format_version": JSON_FORMAT_VERSION,
        "ok": out.problems.is_empty(),
        "integ_lines": ledger.integ_lines,
        "rows": rows,
        "open_noncuttable": out.open_noncuttable,
        "problems": out.problems,
        "notes": out.notes,
        "skipped": out.skipped,
    })
}

/// `cargo xtask release-ledger`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let root = args.repo.as_deref().unwrap_or(root);
    let path = root.join(&args.ledger);
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let ledger = parse(&text)?;
    let git = SystemGit {
        dir: root.to_path_buf(),
    };
    let roots = Roots {
        worktree: root.to_path_buf(),
        scratch: (!args.no_scratch).then(|| args.scratch.clone()),
        ancestry: !args.no_ancestry,
        strict: args.strict,
    };
    let out = check(&ledger, &git, &roots);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&to_json(&ledger, &out))?);
        return Ok(if out.problems.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    if args.print_open {
        let by_id: BTreeMap<&str, &Row> = ledger.rows.iter().map(|r| (r.id.as_str(), r)).collect();
        for id in &out.open_noncuttable {
            if let Some(r) = by_id.get(id.as_str()) {
                println!(
                    "- **{id}** {} ({}; {}): {}",
                    r.item, r.status, r.integration, r.open
                );
            }
        }
    }
    for s in &out.skipped {
        println!("release-ledger: skipped {s}");
    }
    for n in &out.notes {
        println!("release-ledger: note: {n}");
    }
    let mut statuses: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &ledger.rows {
        *statuses.entry(r.status.as_str()).or_default() += 1;
    }
    let summary: Vec<String> = statuses.iter().map(|(s, n)| format!("{s} {n}")).collect();
    println!(
        "release-ledger: {} rows ({}); {} open noncuttable",
        ledger.rows.len(),
        summary.join(", "),
        out.open_noncuttable.len()
    );
    if out.problems.is_empty() {
        println!("release-ledger: ok");
        Ok(ExitCode::SUCCESS)
    } else {
        for p in &out.problems {
            eprintln!("release-ledger: {p}");
        }
        eprintln!("release-ledger: {} problems", out.problems.len());
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake repository: commits, `(commit, path)` trees and `(ancestor, of)` pairs.
    #[derive(Default)]
    struct FakeGit {
        commits: BTreeSet<String>,
        paths: BTreeSet<(String, String)>,
        ancestry: BTreeSet<(String, String)>,
        /// Branch refs → the commit they point at.
        refs: BTreeMap<String, String>,
    }

    impl FakeGit {
        fn commit(mut self, c: &str) -> Self {
            self.commits.insert(c.to_string());
            self
        }
        fn path(mut self, c: &str, p: &str) -> Self {
            self.paths.insert((c.to_string(), p.to_string()));
            self
        }
        fn contains(mut self, line: &str, c: &str) -> Self {
            self.ancestry.insert((c.to_string(), line.to_string()));
            self
        }
        fn branch(mut self, name: &str, c: &str) -> Self {
            self.refs.insert(name.to_string(), c.to_string());
            self
        }
    }

    impl Git for FakeGit {
        fn is_commit(&self, rev: &str) -> bool {
            self.commits.contains(rev)
        }
        fn has_path(&self, rev: &str, path: &str) -> bool {
            self.paths.contains(&(rev.to_string(), path.to_string()))
        }
        fn resolve(&self, rev: &str) -> Option<String> {
            self.refs.get(rev).cloned()
        }
        fn is_ancestor(&self, a: &str, of: &str) -> Option<bool> {
            self.commits
                .contains(of)
                .then(|| a == of || self.ancestry.contains(&(a.to_string(), of.to_string())))
        }
    }

    const HEADER: &str = "| ID | Item | Cut | Status | Job | Lane | Head | Integration | Evidence | Platforms | Open |\n\
                          |---|---|---|---|---|---|---|---|---|---|---|\n";

    /// A ledger whose every id is a `done`, integrated, optional row at `aaaaaaa` citing
    /// `docs/x.md`, except the rows given (id → full row line).
    fn ledger_text(open_list: &[&str], overrides: &[(&str, String)]) -> String {
        let mut s = String::from("# Ledger\n\nInteg lines: `integ-a`, `integ-b`\n\n");
        s.push_str(OPEN_HEADING);
        s.push('\n');
        for id in open_list {
            s.push_str(&format!("- **{id}** something open\n"));
        }
        s.push_str("\n## Ledger\n\n");
        s.push_str(HEADER);
        for id in expected_ids() {
            match overrides.iter().find(|(o, _)| *o == id) {
                Some((_, line)) => s.push_str(line),
                None => s.push_str(&format!(
                    "| {id} | item | optional | done | J00 | lane | `aaaaaaa` | `integ-a` | `docs/x.md` | linux | — |"
                )),
            }
            s.push('\n');
        }
        s
    }

    fn base_git() -> FakeGit {
        FakeGit::default()
            .commit("aaaaaaa")
            .commit("bbbbbbb")
            .commit("integ-a")
            .commit("integ-b")
            .path("aaaaaaa", "docs/x.md")
            .path("bbbbbbb", "docs/y.md")
            .contains("integ-a", "aaaaaaa")
    }

    fn roots(scratch: Option<PathBuf>) -> Roots {
        Roots {
            worktree: PathBuf::from("/nonexistent-worktree"),
            scratch,
            ancestry: true,
            strict: false,
        }
    }

    fn problems(text: &str, git: &FakeGit) -> Vec<String> {
        let ledger = parse(text).expect("parse");
        check(&ledger, git, &roots(None)).problems
    }

    #[test]
    fn expected_ids_are_the_84_burndown_rows_in_order() {
        let ids = expected_ids();
        assert_eq!(ids.len(), 84);
        assert_eq!(ids.first().map(String::as_str), Some("L01"));
        assert_eq!(ids.last().map(String::as_str), Some("X06"));
        assert!(ids.contains(&"B20".to_string()) && !ids.contains(&"B21".to_string()));
    }

    #[test]
    fn a_consistent_ledger_passes() {
        let text = ledger_text(&[], &[]);
        assert_eq!(problems(&text, &base_git()), Vec::<String>::new());
    }

    #[test]
    fn missing_duplicate_and_unknown_ids_fail() {
        let mut text = ledger_text(&[], &[]);
        text = text.replace("| L02 |", "| L01 |");
        text.push_str("| Z99 | item | optional | done | J00 | lane | `aaaaaaa` | `integ-a` | `docs/x.md` | linux | — |\n");
        let p = problems(&text, &base_git());
        assert!(p.iter().any(|x| x.contains("L01: duplicate id")), "{p:?}");
        assert!(
            p.iter()
                .any(|x| x.starts_with("L02: burn-down id has no ledger row")),
            "{p:?}"
        );
        assert!(p.iter().any(|x| x.contains("Z99: unknown burn-down id")), "{p:?}");
    }

    #[test]
    fn a_head_that_is_not_a_commit_fails() {
        let row =
            "| L03 | item | optional | done | J05 | lane | `deadbee` | `integ-a` | `docs/x.md` | linux | — |"
                .to_string();
        let p = problems(&ledger_text(&[], &[("L03", row)]), &base_git());
        assert!(
            p.iter()
                .any(|x| x.contains("L03: head `deadbee` is not a commit")),
            "{p:?}"
        );
    }

    #[test]
    fn a_malformed_head_fails() {
        let row =
            "| L03 | item | optional | done | J05 | lane | `main` | `integ-a` | `docs/x.md` | linux | — |"
                .to_string();
        let p = problems(&ledger_text(&[], &[("L03", row)]), &base_git());
        assert!(
            p.iter().any(|x| x.contains("not a 7-40 digit hex commit")),
            "{p:?}"
        );
    }

    #[test]
    fn evidence_is_checked_in_the_tree_of_the_rows_head() {
        // `docs/y.md` exists at bbbbbbb but not at aaaaaaa: citing it on an aaaaaaa row fails.
        let row =
            "| M01 | item | optional | done | J02 | lane | `aaaaaaa` | `integ-a` | `docs/y.md` | linux | — |"
                .to_string();
        let p = problems(&ledger_text(&[], &[("M01", row)]), &base_git());
        assert!(
            p.iter()
                .any(|x| x.contains("M01: evidence `docs/y.md` is not in the tree of aaaaaaa")),
            "{p:?}"
        );
    }

    #[test]
    fn absolute_evidence_paths_are_rejected() {
        let row = "| M01 | item | optional | done | J02 | lane | `aaaaaaa` | `integ-a` | `/etc/passwd` | linux | — |".to_string();
        let p = problems(&ledger_text(&[], &[("M01", row)]), &base_git());
        assert!(p.iter().any(|x| x.contains("must be repo-relative")), "{p:?}");
    }

    #[test]
    fn scratch_evidence_resolves_under_the_scratch_root_or_is_reported_skipped() {
        let dir = regia_test_utils::unique_temp_dir("release-ledger-scratch");
        let reports = dir.join("reports");
        std::fs::create_dir_all(&reports).expect("mkdir");
        std::fs::write(reports.join("lane.md"), "Status: DONE\n").expect("write");
        let good = "| A01 | item | optional | done | J06 | lane | `aaaaaaa` | `integ-a` | `scratch:reports/lane.md` | linux | — |".to_string();
        let bad = "| A02 | item | optional | done | J06 | lane | `aaaaaaa` | `integ-a` | `scratch:reports/gone.md` | linux | — |".to_string();
        let text = ledger_text(&[], &[("A01", good), ("A02", bad)]);
        let ledger = parse(&text).expect("parse");
        let with = check(&ledger, &base_git(), &roots(Some(dir.clone())));
        assert_eq!(with.problems.len(), 1, "{:?}", with.problems);
        assert!(with.problems[0].contains("A02: evidence `scratch:reports/gone.md` not found"));
        let without = check(&ledger, &base_git(), &roots(None));
        assert!(without.problems.is_empty(), "{:?}", without.problems);
        assert_eq!(
            without.skipped,
            vec!["2 scratch evidence paths (--no-scratch)".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_head_outside_every_integ_line_must_say_pending_integration() {
        // bbbbbbb is in no integration line: claiming `integ-a` fails, `pending integration` passes.
        let claim =
            "| B04 | item | optional | done | J16 | lane | `bbbbbbb` | `integ-a` | `docs/y.md` | linux | — |"
                .to_string();
        let p = problems(&ledger_text(&[], &[("B04", claim)]), &base_git());
        assert!(
            p.iter().any(|x| x.contains("B04: bbbbbbb is not in `integ-a`")),
            "{p:?}"
        );
        let honest = "| B04 | item | optional | done | J16 | lane | `bbbbbbb` | pending integration | `docs/y.md` | linux | merge |".to_string();
        assert_eq!(
            problems(&ledger_text(&[], &[("B04", honest)]), &base_git()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_pending_row_that_was_integrated_since_fails() {
        let stale = "| B04 | item | optional | done | J16 | lane | `aaaaaaa` | pending integration | `docs/x.md` | linux | merge |".to_string();
        let p = problems(&ledger_text(&[], &[("B04", stale)]), &base_git());
        assert!(
            p.iter()
                .any(|x| x.contains("says `pending integration` but aaaaaaa is in `integ-a`")),
            "{p:?}"
        );
    }

    #[test]
    fn a_row_without_a_head_must_be_pending() {
        let nohead =
            "| D03 | item | optional | not-run | J09 | — | — | `integ-a` | — | — | not run |".to_string();
        let p = problems(&ledger_text(&[], &[("D03", nohead)]), &base_git());
        assert!(
            p.iter()
                .any(|x| x.contains("D03: a row without a head must say `pending integration`")),
            "{p:?}"
        );
    }

    #[test]
    fn an_unresolvable_integ_line_fails_and_no_ancestry_skips_it() {
        let text = ledger_text(&[], &[]).replace("`integ-b`", "`integ-gone`");
        let ledger = parse(&text).expect("parse");
        let p = check(&ledger, &base_git(), &roots(None)).problems;
        assert!(
            p.iter().any(|x| x.contains("`integ-gone` does not resolve")),
            "{p:?}"
        );
        let r = Roots {
            ancestry: false,
            ..roots(None)
        };
        let skipped = check(&ledger, &base_git(), &r);
        assert!(skipped.problems.is_empty(), "{:?}", skipped.problems);
        assert!(skipped.skipped.iter().any(|s| s.contains("--no-ancestry")));
    }

    #[test]
    fn done_needs_evidence_and_open_needs_an_open_note() {
        let done = "| C01 | item | optional | done | J15 | lane | `aaaaaaa` | `integ-a` | — | linux | — |"
            .to_string();
        let open = "| C02 | item | optional | partial | J22 | lane | `aaaaaaa` | `integ-a` | `docs/x.md` | linux | — |".to_string();
        let p = problems(&ledger_text(&[], &[("C01", done), ("C02", open)]), &base_git());
        assert!(
            p.iter()
                .any(|x| x.contains("C01: a `done` row must cite evidence")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.contains("C02: an open row must say what is still open")),
            "{p:?}"
        );
    }

    #[test]
    fn bad_status_and_cut_words_fail() {
        let row =
            "| N01 | item | maybe | green | J07 | lane | `aaaaaaa` | `integ-a` | `docs/x.md` | linux | x |"
                .to_string();
        let p = problems(&ledger_text(&[], &[("N01", row)]), &base_git());
        assert!(p.iter().any(|x| x.contains("cut `maybe`")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("status `green`")), "{p:?}");
    }

    #[test]
    fn the_open_noncuttable_list_must_match_the_table() {
        let open = "| N05 | item | noncuttable | blocked | J19 | lane | `bbbbbbb` | pending integration | `docs/y.md` | linux | owner run |".to_string();
        let integrated_done = "| N04 | item | noncuttable | done | J07 | lane | `aaaaaaa` | `integ-a` | `docs/x.md` | linux | — |".to_string();
        let rows = [("N05", open.clone()), ("N04", integrated_done.clone())];
        // Listed correctly: passes.
        assert_eq!(
            problems(&ledger_text(&["N05"], &rows), &base_git()),
            Vec::<String>::new()
        );
        // Missing from the list.
        let p = problems(&ledger_text(&[], &rows), &base_git());
        assert!(
            p.iter().any(|x| x.contains("N05: open noncuttable row missing")),
            "{p:?}"
        );
        // A closed row listed as open.
        let p = problems(&ledger_text(&["N05", "N04"], &rows), &base_git());
        assert!(p.iter().any(|x| x.contains("N04: listed under")), "{p:?}");
    }

    #[test]
    fn done_but_pending_integration_counts_as_open() {
        let row = "| N07 | item | noncuttable | done | J07 | lane | `bbbbbbb` | pending integration | `docs/y.md` | linux | merge |".to_string();
        let text = ledger_text(&["N07"], &[("N07", row)]);
        let ledger = parse(&text).expect("parse");
        let out = check(&ledger, &base_git(), &roots(None));
        assert!(out.problems.is_empty(), "{:?}", out.problems);
        assert_eq!(out.open_noncuttable, vec!["N07".to_string()]);
    }

    fn tip_ledger() -> String {
        let row = "| N04 | item | optional | done | J07 | net-a | `aaaaaaa` | `integ-a` | `docs/x.md` | linux | — |";
        ledger_text(&[], &[("N04", row.to_string())])
    }

    #[test]
    fn a_lane_tip_at_the_cited_head_gives_no_note() {
        let git = base_git().branch("origin/lane/sprint-net-a", "aaaaaaa1234");
        let out = check(&parse(&tip_ledger()).expect("parse"), &git, &roots(None));
        assert!(out.problems.is_empty(), "{:?}", out.problems);
        assert!(!out.notes.iter().any(|n| n.contains("N04")), "{:?}", out.notes);
    }

    #[test]
    fn a_lane_tip_that_moved_or_vanished_is_a_note_and_strict_makes_it_a_failure() {
        let moved = base_git().branch("origin/lane/sprint-net-a", "bbbbbbb9999");
        let ledger = parse(&tip_ledger()).expect("parse");
        let out = check(&ledger, &moved, &roots(None));
        assert!(out.problems.is_empty(), "{:?}", out.problems);
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("N04: lane net-a moved: `origin/lane/sprint-net-a` is at bbbbbbb9999")),
            "{:?}",
            out.notes
        );
        let gone = check(&ledger, &base_git(), &roots(None));
        assert!(
            gone.notes
                .iter()
                .any(|n| n.contains("N04: lane net-a has no `origin/lane/sprint-net-a`")),
            "{:?}",
            gone.notes
        );
        let strict = Roots {
            strict: true,
            ..roots(None)
        };
        let out = check(&ledger, &moved, &strict);
        assert!(
            out.problems.iter().any(|p| p.contains("N04: lane net-a moved")),
            "{:?}",
            out.problems
        );
    }

    /// ADR-5806: after branch cleanup a merged lane's branch is gone; on 2026-11-11 that made 83 of
    /// 84 rows fail `--strict`. A vanished branch whose cited head is in a listed integ line is a
    /// note even under `--strict`; one whose head is in no line still fails.
    #[test]
    fn a_deleted_branch_of_a_merged_head_is_not_a_strict_failure() {
        let strict = Roots {
            strict: true,
            ..roots(None)
        };
        let merged = check(&parse(&tip_ledger()).expect("parse"), &base_git(), &strict);
        assert!(merged.problems.is_empty(), "{:?}", merged.problems);
        assert!(
            merged.notes.iter().any(|n| n.contains(
                "N04: lane net-a has no `origin/lane/sprint-net-a`; its head aaaaaaa is in `integ-a` (merged, branch deleted)"
            )),
            "{:?}",
            merged.notes
        );
        let row = "| N04 | item | optional | partial | J07 | net-a | `bbbbbbb` | pending integration | `docs/y.md` | linux | open |";
        let unmerged = ledger_text(&[], &[("N04", row.to_string())]);
        let out = check(&parse(&unmerged).expect("parse"), &base_git(), &strict);
        assert!(
            out.problems.iter().any(|p| p
                .contains("N04: lane net-a has no `origin/lane/sprint-net-a` (push it or cite another ref)")),
            "{:?}",
            out.problems
        );
    }

    #[test]
    fn tips_are_not_read_without_ancestry() {
        let moved = base_git().branch("origin/lane/sprint-net-a", "bbbbbbb9999");
        let r = Roots {
            ancestry: false,
            ..roots(None)
        };
        let out = check(&parse(&tip_ledger()).expect("parse"), &moved, &r);
        assert!(out.notes.is_empty(), "{:?}", out.notes);
    }

    #[test]
    fn json_output_carries_rows_open_ids_problems_and_notes() {
        let open = "| N05 | Steam | noncuttable | blocked | J19 | steam-b | `bbbbbbb` | pending integration | `docs/y.md` | linux | owner run |";
        let text = ledger_text(&["N05"], &[("N05", open.to_string())]);
        let ledger = parse(&text).expect("parse");
        let out = check(&ledger, &base_git(), &roots(None));
        let v = to_json(&ledger, &out);
        assert_eq!(v["format_version"], 1);
        assert_eq!(v["ok"], true);
        assert_eq!(v["rows"].as_array().map(Vec::len), Some(84));
        assert_eq!(v["open_noncuttable"], serde_json::json!(["N05"]));
        let n05 = v["rows"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["id"] == "N05"))
            .cloned()
            .expect("N05 row");
        assert_eq!(n05["head"], "bbbbbbb");
        assert_eq!(n05["integration"], PENDING);
        assert_eq!(n05["is_open"], true);
        assert_eq!(n05["evidence"], serde_json::json!(["docs/y.md"]));
        // Every lane branch is missing in the fake repository: one note per row with a head.
        assert_eq!(v["notes"].as_array().map(Vec::len), Some(84));
        let bad = check(
            &parse(&ledger_text(&[], &[("N05", open.to_string())])).expect("parse"),
            &base_git(),
            &roots(None),
        );
        let v = to_json(&ledger, &bad);
        assert_eq!(v["ok"], false);
        assert!(v["problems"].as_array().is_some_and(|p| !p.is_empty()));
    }

    #[test]
    fn a_malformed_job_id_fails() {
        let row = "| N01 | item | optional | done | job5 | lane | `aaaaaaa` | `integ-a` | `docs/x.md` | linux | — |".to_string();
        let p = problems(&ledger_text(&[], &[("N01", row)]), &base_git());
        assert!(
            p.iter()
                .any(|x| x.contains("N01: job `JOB5` is not a burn-down job id")),
            "{p:?}"
        );
    }

    #[test]
    fn supporting_lanes_need_a_real_head_and_report_once_each() {
        let mut text = ledger_text(&[], &[]);
        text.push_str(
            "\n## Supporting lanes\n\n| Lane | Head | Status | What | Report |\n|---|---|---|---|---|\n",
        );
        text.push_str("| relost-01 | `bbbbbbb` | DONE | x | `docs/y.md` |\n");
        assert_eq!(problems(&text, &base_git()), Vec::<String>::new());
        text.push_str("| relost-01 | `deadbee` | DONE | x | `docs/y.md` |\n");
        text.push_str("| relost-02 | — | DONE | x | — |\n");
        let p = problems(&text, &base_git());
        assert!(
            p.iter().any(|x| x.contains("relost-01: lane listed twice")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.contains("relost-01: head `deadbee` is not a commit")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.contains("relost-01: evidence `docs/y.md` is not in the tree of deadbee")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.contains("relost-02: a supporting lane must cite its head")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.contains("relost-02: a supporting lane must cite its report")),
            "{p:?}"
        );
    }

    #[test]
    fn a_table_without_a_required_column_is_a_parse_error() {
        // Every column but `Head`, so the error names exactly the one that is missing.
        let text = "## Ledger\n\n| ID | Item | Cut | Status | Job | Lane | Integration | Evidence | Platforms | Open |\n\
                    |---|---|---|---|---|---|---|---|---|---|\n";
        let err = parse(text).expect_err("missing columns");
        assert!(format!("{err}").contains("no `head` column"), "{err}");
        assert!(parse("# nothing\n").is_err());
    }

    /// Answers yes to every commit and path question; ancestry is never asked (`ancestry: false`).
    struct AnyGit;

    impl Git for AnyGit {
        fn is_commit(&self, _rev: &str) -> bool {
            true
        }
        fn has_path(&self, _rev: &str, _path: &str) -> bool {
            true
        }
        fn is_ancestor(&self, _a: &str, _of: &str) -> Option<bool> {
            None
        }
        fn resolve(&self, _rev: &str) -> Option<String> {
            None
        }
    }

    /// The committed ledger is internally consistent: all 84 ids, valid words and job ids, `done`
    /// rows with evidence, open rows with a note, and the top list equal to the table's open
    /// noncuttable rows. Git and scratch checks need the lead's refs and disk, so they are stubbed
    /// here; `cargo xtask release-ledger` runs them for real.
    #[test]
    fn the_committed_ledger_is_internally_consistent() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let text = std::fs::read_to_string(repo.join(LEDGER)).expect("read the committed ledger");
        let ledger = parse(&text).expect("parse the committed ledger");
        assert_eq!(ledger.rows.len(), 84);
        assert!(!ledger.integ_lines.is_empty());
        let r = Roots {
            worktree: repo,
            scratch: None,
            ancestry: false,
            strict: false,
        };
        let out = check(&ledger, &AnyGit, &r);
        assert!(out.problems.is_empty(), "{:#?}", out.problems);
        assert_eq!(out.open_noncuttable.len(), ledger.open_listed.len());
    }

    /// The real [`SystemGit`] against a throwaway repository: commit, path and ancestry checks.
    #[test]
    fn system_git_answers_commit_path_and_ancestry_questions() {
        let dir = regia_test_utils::unique_temp_dir("release-ledger-git");
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .map(|o| {
                    (
                        o.status.success(),
                        String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    )
                })
        };
        let Ok((true, _)) = git(&["init", "-q"]) else {
            eprintln!("git unavailable; skipping");
            return;
        };
        std::fs::create_dir_all(dir.join("docs")).expect("mkdir");
        std::fs::write(dir.join("docs/a.md"), "a\n").expect("write");
        assert!(matches!(git(&["add", "."]), Ok((true, _))));
        assert!(matches!(git(&["commit", "-q", "-m", "one"]), Ok((true, _))));
        let (_, first) = git(&["rev-parse", "HEAD"]).expect("rev-parse");
        std::fs::write(dir.join("docs/b.md"), "b\n").expect("write");
        assert!(matches!(git(&["add", "."]), Ok((true, _))));
        assert!(matches!(git(&["commit", "-q", "-m", "two"]), Ok((true, _))));
        let (_, second) = git(&["rev-parse", "HEAD"]).expect("rev-parse");

        let sys = SystemGit { dir: dir.clone() };
        assert!(sys.is_commit(&first) && sys.is_commit(&second));
        assert!(!sys.is_commit("0000000000000000000000000000000000000000"));
        assert!(sys.has_path(&first, "docs/a.md"));
        assert!(!sys.has_path(&first, "docs/b.md"));
        assert!(sys.has_path(&second, "docs/b.md"));
        assert_eq!(sys.is_ancestor(&first, &second), Some(true));
        assert_eq!(sys.is_ancestor(&second, &first), Some(false));
        assert_eq!(sys.is_ancestor(&first, "no-such-ref"), None);
        assert_eq!(sys.resolve("HEAD").as_deref(), Some(second.as_str()));
        assert_eq!(sys.resolve(&first[..9]).as_deref(), Some(first.as_str()));
        assert_eq!(sys.resolve("no-such-ref"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
