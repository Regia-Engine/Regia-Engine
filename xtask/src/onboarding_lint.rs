// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask onboarding-lint`: the user-facing onboarding files agree with the onboarding contract
//! (`xtask/onboarding/contract.md` on the v0.6 line, numbered like spec 125's sections; ADR-10135, ADR-10230)
//! and with the `regia` CLI.
//!
//! The files: the export overlay `dist/public-root/` (the user `AGENTS.md`, `CLAUDE.md`, `README.md`, the
//! MCP template under `dist/mcp/`), the Claude Code skills' sources under `dist/public-skills/` (the export
//! writes them to `.claude/skills/`) and the two book pages of the assistant path
//! (`book/start/assistant-setup.md`, `book/agents/onboarding.md`).
//! Each finding carries a stable code (documented on `book/agents/onboarding.md`):
//!
//! - **Question labels** (`question_id_mismatch`, `question_total_mismatch`, `question_missing`,
//!   `question_order`, `label_template_missing`): every `Question N of <total> · <id>` label names the id
//!   the contract's § 7 gives that number; the total is the number of numbered questions; Appendix A, the
//!   user `AGENTS.md`, `book/agents/onboarding.md` and the `regia-setup` skill name every question in § 7
//!   order; `AGENTS.md` states both label templates.
//! - **Consent labels** (`consent_step_unknown`): every `Consent · <step>` names a step of § 6.3.
//! - **Commands** (`command_unknown`, `pending_command_exists`): every `regia <subcommand>` in code
//!   (inline or fenced) is a command of `regia_cli` (read from source as `book-coverage` does) or is
//!   listed in `xtask/onboarding/pending-commands.txt`; a listed command that exists fails, so the
//!   list shrinks as the commands land.
//! - **Shape** (`agents_md_marker_missing`, `agents_md_too_long`, `agents_md_sections`,
//!   `claude_md_differs`, `skill_missing`, `skill_unknown`, `skill_front_matter`, `skill_too_long`,
//!   `skill_no_agents_link`, `skill_duplicates_rule`, `skill_in_overlay`, `mcp_template_invalid`,
//!   `onboarding_file_missing`): the marker line, at most 200 lines and Appendix A's sections in order for
//!   `AGENTS.md`; `CLAUDE.md` equal to § 4.2; the skills of § 5.1, each with `name` equal to its folder, a
//!   description, a body of at most 120 lines, a link to `AGENTS.md` and no rule copied from it; no
//!   `.claude/` folder inside the overlay (it would load as live skills in sessions on this repository);
//!   the MCP template parses.
//! - **Prerequisites** (`prerequisites_differ`, `install_row_missing`): the per-OS table of the user
//!   `AGENTS.md` names the same backticked items per OS as § 6.1's table, and every distribution its
//!   Linux row names has a row in `book/start/install.md`.
//! - **Links** (`link_missing`): every relative link of an overlay or skill Markdown file resolves in the
//!   overlay or the repository (the export copies the overlay over the root and the skills to
//!   `.claude/skills/`).
//! - **Forbidden terms** (`forbidden_term`): no hand-written overlay or skill file contains a term of
//!   `xtask/onboarding/forbidden-terms.txt` (a private list the export leaves out). The generated
//!   `llms.txt` and `llms-full.txt` are the book's text and are checked by the export's own scan.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use regex::Regex;

use crate::book_coverage::{cli_commands, cli_sources, nested_cli_commands, row_cells};

/// The onboarding contract, the source of the ids, steps, skills, Appendix A and the `CLAUDE.md` text
/// (ADR-10230; spec 125 takes its place when the v0.8 guided setup lands).
pub const SPEC: &str = "xtask/onboarding/contract.md";
/// The export overlay.
pub const OVERLAY: &str = "dist/public-root";
/// The user skills' sources, one `<name>/SKILL.md` per skill; the export writes them to `.claude/skills/`.
pub const SKILLS: &str = "dist/public-skills";
/// Where the export puts the skills in the snapshot (links in a skill resolve from there).
pub const SKILLS_PUBLISHED: &str = ".claude/skills";
/// Commands the docs name before they exist (one `regia` command path per line, `#` comments).
pub const PENDING: &str = "xtask/onboarding/pending-commands.txt";
/// Terms that must not appear in overlay text (one per line, `#` comments; private).
pub const FORBIDDEN: &str = "xtask/onboarding/forbidden-terms.txt";
/// The agent-guide page of the setup flow.
pub const BOOK_ONBOARDING: &str = "book/agents/onboarding.md";
/// The user page of the assistant path.
pub const BOOK_ASSISTANT: &str = "book/start/assistant-setup.md";
/// The hand-install page.
pub const BOOK_INSTALL: &str = "book/start/install.md";
/// The marker the export checks for on the user `AGENTS.md`.
pub const AGENTS_MARKER: &str = "<!-- regia:user-agents-md -->";
/// Longest user `AGENTS.md`, in lines.
pub const AGENTS_MAX_LINES: usize = 200;
/// Longest skill body (after the front matter), in lines.
pub const SKILL_MAX_LINES: usize = 120;
/// The question label template `AGENTS.md` states, for a script of `total` numbered questions.
pub fn question_template(total: usize) -> String {
    format!("(Question N of {total} · <id>)")
}
/// The consent label template `AGENTS.md` states.
pub const CONSENT_TEMPLATE: &str = "(Consent · <step id>)";
/// The skill that runs the whole question script.
pub const SETUP_SKILL: &str = "regia-setup";

/// One finding: a stable code, the file and the message.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// Lowercase code.
    pub code: &'static str,
    /// Repository-relative file.
    pub file: String,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}: {}", self.code, self.file, self.message)
    }
}

/// What the onboarding contract fixes.
#[derive(Debug, Clone, Default)]
pub struct SpecFacts {
    /// § 7 questions in order: (number as written after `Q`, id).
    pub questions: Vec<(String, String)>,
    /// Count of numbered (digit-only) questions: the `N of <total>`.
    pub total: usize,
    /// § 6.3 step ids.
    pub steps: BTreeSet<String>,
    /// § 5.1 skill names.
    pub skills: Vec<String>,
    /// Appendix A's Markdown.
    pub appendix_a: String,
    /// § 4.2's `CLAUDE.md`.
    pub claude_md: String,
    /// § 6.1's prerequisite table: OS → backticked items.
    pub prerequisites: BTreeMap<String, BTreeSet<String>>,
}

/// The rows of the first table after a header whose leading cells are `first`.
fn rows_after(text: &str, first: &[&str]) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        if !line.trim_start().starts_with('|') {
            if in_table {
                break;
            }
            continue;
        }
        let cells = row_cells(line);
        if !in_table {
            in_table = cells.len() >= first.len() && cells.iter().zip(first).all(|(c, f)| c == f);
            continue;
        }
        if cells
            .iter()
            .all(|c| c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
        {
            continue;
        }
        rows.push(cells);
    }
    rows
}

/// The text from the heading line starting with `heading` up to the next heading of the same level.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let Some(start) = text.find(&format!("\n{heading}")).map(|i| i + 1) else {
        return "";
    };
    let level = heading.chars().take_while(|c| *c == '#').count();
    let marker = format!("\n{} ", "#".repeat(level));
    let body_start = start + heading.len();
    let end = text[body_start..]
        .find(&marker)
        .map_or(text.len(), |i| body_start + i);
    &text[start..end]
}

/// The backticked spans of `cell`.
fn ticks(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The body of the first fence opened with `opener` (```` ````markdown ```` or ```` ```markdown ````).
fn fence_body(text: &str, opener: &str) -> Option<String> {
    let start = text.find(opener)?;
    let after = start + text[start..].find('\n')? + 1;
    let ticks_len = opener.chars().take_while(|c| *c == '`').count();
    let close = format!("\n{}\n", "`".repeat(ticks_len));
    let end = text[after - 1..].find(&close).map(|i| after - 1 + i)?;
    Some(text[after..=end].to_string())
}

/// Reads the facts of the contract.
pub fn spec_facts(spec: &str) -> SpecFacts {
    let mut facts = SpecFacts::default();
    for row in rows_after(section(spec, "## 7."), &["#", "Id"]) {
        let (Some(n), Some(id)) = (row.first(), row.get(1).and_then(|c| ticks(c).into_iter().next())) else {
            continue;
        };
        let Some(number) = n.strip_prefix('Q') else {
            continue;
        };
        if number.chars().all(|c| c.is_ascii_digit()) {
            facts.total += 1;
        }
        facts.questions.push((number.to_string(), id));
    }
    for row in rows_after(section(spec, "### 6.3"), &["Id", "Does"]) {
        if let Some(cell) = row.first() {
            facts.steps.extend(ticks(cell));
        }
    }
    for row in rows_after(section(spec, "### 5.1"), &["Skill"]) {
        if let Some(name) = row.first().and_then(|c| ticks(c).into_iter().next()) {
            facts.skills.push(name);
        }
    }
    // Appendix A's fence holds `## ` headings of its own: take the fence, not the section.
    facts.appendix_a = spec
        .find("\n## Appendix A")
        .and_then(|at| fence_body(&spec[at..], "````markdown"))
        .unwrap_or_default();
    facts.claude_md = fence_body(section(spec, "### 4.2"), "```markdown").unwrap_or_default();
    for row in rows_after(section(spec, "### 6.1"), &["OS"]) {
        if let (Some(os), Some(cell)) = (row.first(), row.get(1)) {
            facts
                .prerequisites
                .insert(os.clone(), ticks(cell).into_iter().collect());
        }
    }
    facts
}

/// A question label found in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Label {
    number: String,
    total: Option<usize>,
    id: String,
    line: usize,
}

fn question_re() -> Regex {
    // `Question 3 of 7 · install_from`, `Question 5b · client`, the template `Question N of 7 · <id>`.
    Regex::new(r"Question\s+([0-9]+[a-z]?|N)(?:\s+of\s+([0-9]+))?\s+·\s+(<id>|[a-z_]+)").expect("valid regex")
}

fn consent_re() -> Regex {
    Regex::new(r"Consent\s+·\s+(<step id>|[a-z_]+)").expect("valid regex")
}

/// Every concrete question label of `text` (templates skipped).
fn labels(text: &str) -> Vec<Label> {
    let re = question_re();
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for c in re.captures_iter(line) {
            let number = c[1].to_string();
            let id = c[3].to_string();
            if number == "N" || id == "<id>" {
                continue;
            }
            out.push(Label {
                number,
                total: c.get(2).and_then(|m| m.as_str().parse().ok()),
                id,
                line: i + 1,
            });
        }
    }
    out
}

/// Checks the question and consent labels of one document. `complete` requires every § 7 question
/// in order.
fn check_labels(facts: &SpecFacts, file: &str, text: &str, complete: bool, out: &mut Vec<Finding>) {
    let by_number: BTreeMap<&str, &str> = facts
        .questions
        .iter()
        .map(|(n, i)| (n.as_str(), i.as_str()))
        .collect();
    let found = labels(text);
    for l in &found {
        match by_number.get(l.number.as_str()) {
            Some(id) if *id == l.id => {}
            Some(id) => out.push(Finding {
                code: "question_id_mismatch",
                file: file.into(),
                message: format!(
                    "line {}: `Question {} · {}`, but the contract § 7 has Q{} = `{id}`",
                    l.line, l.number, l.id, l.number
                ),
            }),
            None => out.push(Finding {
                code: "question_id_mismatch",
                file: file.into(),
                message: format!(
                    "line {}: `Question {}` is not a question of the contract § 7",
                    l.line, l.number
                ),
            }),
        }
        if let Some(total) = l.total
            && total != facts.total
        {
            out.push(Finding {
                code: "question_total_mismatch",
                file: file.into(),
                message: format!(
                    "line {}: `of {total}`, but the contract § 7 has {} questions",
                    l.line, facts.total
                ),
            });
        }
    }
    if complete {
        let mut order: Vec<&str> = Vec::new();
        for l in &found {
            if !order.contains(&l.id.as_str()) {
                order.push(&l.id);
            }
        }
        for (n, id) in &facts.questions {
            if !order.contains(&id.as_str()) {
                out.push(Finding {
                    code: "question_missing",
                    file: file.into(),
                    message: format!("no label for Q{n} `{id}`"),
                });
            }
        }
        let expected: Vec<&str> = facts
            .questions
            .iter()
            .map(|(_, id)| id.as_str())
            .filter(|id| order.contains(id))
            .collect();
        let seen: Vec<&str> = order.iter().copied().filter(|id| expected.contains(id)).collect();
        if seen != expected {
            out.push(Finding {
                code: "question_order",
                file: file.into(),
                message: format!(
                    "questions first appear as {seen:?}, the contract § 7 order is {expected:?}"
                ),
            });
        }
    }
    let consent = consent_re();
    for (i, line) in text.lines().enumerate() {
        for c in consent.captures_iter(line) {
            let step = &c[1];
            if step != "<step id>" && !facts.steps.contains(step) {
                out.push(Finding {
                    code: "consent_step_unknown",
                    file: file.into(),
                    message: format!(
                        "line {}: `Consent · {step}` names no step of the contract § 6.3",
                        i + 1
                    ),
                });
            }
        }
    }
}

/// The code text of a Markdown document: fenced blocks whole, inline code spans, one string per
/// line or span, with the line number.
pub fn code_segments(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fence = false;
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            out.push((i + 1, line.to_string()));
            continue;
        }
        for span in line.split('`').skip(1).step_by(2) {
            if !span.trim().is_empty() {
                out.push((i + 1, span.to_string()));
            }
        }
    }
    out
}

/// Every `regia <word> [<word>]` command path named in code segments of `text`, with its line.
/// `cargo run -p regia_cli -- <word>` counts as `regia <word>`.
pub fn command_mentions(text: &str) -> Vec<(usize, Vec<String>)> {
    let re =
        Regex::new(r#"(?:^|[\s/\\("'$=])regia(?:\.exe)?[ \t]+([a-z][a-z0-9-]*)(?:[ \t]+([a-z][a-z0-9-]*))?"#)
            .expect("valid regex");
    let cargo = Regex::new(r"-p[ \t]+regia_cli[ \t]+--[ \t]+([a-z][a-z0-9-]*)(?:[ \t]+([a-z][a-z0-9-]*))?")
        .expect("valid regex");
    let mut out = Vec::new();
    for (line, code) in code_segments(text) {
        for re in [&re, &cargo] {
            for c in re.captures_iter(&code) {
                let mut words = vec![c[1].to_string()];
                if let Some(second) = c.get(2) {
                    words.push(second.as_str().to_string());
                }
                out.push((line, words));
            }
        }
    }
    out
}

/// The `regia` CLI surface: top-level commands and nested command paths, read from source.
#[derive(Debug, Clone, Default)]
pub struct CliSurface {
    /// Top-level subcommands.
    pub commands: BTreeSet<String>,
    /// Nested paths (`agent call`, `mcp token`).
    pub nested: BTreeSet<String>,
}

impl CliSurface {
    /// Reads `crates/regia_cli/src`.
    pub fn read(root: &Path) -> Result<Self> {
        let main_path = root.join("crates/regia_cli/src/main.rs");
        let main = std::fs::read_to_string(&main_path)
            .with_context(|| format!("reading {}", main_path.display()))?;
        let mut sources = cli_sources(root)?;
        sources.push(main.clone());
        Ok(Self {
            commands: cli_commands(&main)
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .collect(),
            nested: nested_cli_commands(&main, &sources)
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .collect(),
        })
    }

    /// Whether a command path (`setup`, `mcp token`) exists.
    pub fn has(&self, path: &str) -> bool {
        if path.contains(' ') {
            self.nested.contains(path)
        } else {
            self.commands.contains(path)
        }
    }

    fn has_children(&self, top: &str) -> bool {
        let prefix = format!("{top} ");
        self.nested.iter().any(|p| p.starts_with(&prefix))
    }
}

/// Reads a one-entry-per-line list with `#` comments.
fn read_list(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn check_commands(
    cli: &CliSurface,
    pending: &BTreeSet<String>,
    file: &str,
    text: &str,
    out: &mut Vec<Finding>,
) {
    for (line, words) in command_mentions(text) {
        let top = &words[0];
        if pending.contains(top) {
            continue;
        }
        let pair = words.get(1).map(|w| format!("{top} {w}"));
        if pair.as_ref().is_some_and(|p| pending.contains(p)) {
            continue;
        }
        if !cli.has(top) {
            out.push(Finding {
                code: "command_unknown",
                file: file.into(),
                message: format!("line {line}: `regia {top}` is not a `regia` command and not in {PENDING}"),
            });
            continue;
        }
        if let Some(pair) = pair
            && cli.has_children(top)
            && !cli.has(&pair)
        {
            out.push(Finding {
                code: "command_unknown",
                file: file.into(),
                message: format!("line {line}: `regia {pair}` is not a `regia` command and not in {PENDING}"),
            });
        }
    }
}

/// Whether `term` occurs in `lower` (already lowercase): whole-word for word-only terms, substring
/// otherwise.
fn has_term(lower: &str, term: &str) -> bool {
    let term = term.to_lowercase();
    let wordy = term.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !wordy {
        return lower.contains(&term);
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    lower.match_indices(&term).any(|(i, m)| {
        let before = lower[..i].chars().next_back();
        let after = lower[i + m.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// The H2 headings of `text` outside fences.
fn h2s(text: &str) -> Vec<String> {
    let mut fence = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if !fence && let Some(h) = line.strip_prefix("## ") {
            out.push(h.trim().to_string());
        }
    }
    out
}

/// Splits `---` front matter into `key: value` pairs and the body.
pub fn front_matter(text: &str) -> Option<(BTreeMap<String, String>, String)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let mut map = BTreeMap::new();
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
        }
    }
    Some((map, rest[end + 5..].to_string()))
}

/// The numbered rules of the `## Rules` section, whitespace-normalized.
fn rules(agents: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in section(agents, "## Rules").lines().skip(1) {
        let t = line.trim();
        let numbered = t
            .split_once(". ")
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if let Some((_, rest)) = numbered {
            out.push(rest.to_string());
        } else if !t.is_empty()
            && line.starts_with(' ')
            && let Some(last) = out.last_mut()
        {
            last.push(' ');
            last.push_str(t);
        }
    }
    out.into_iter()
        .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// The OS rows of the first `| OS |` table of `text`.
fn prerequisite_rows(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    rows_after(text, &["OS"])
        .into_iter()
        .filter_map(|r| Some((r.first()?.clone(), ticks(r.get(1)?).into_iter().collect())))
        .collect()
}

/// Every relative Markdown link target of `text` outside code.
fn relative_links(text: &str) -> Vec<(usize, String)> {
    let re = Regex::new(r"\]\(([^)\s]+)").expect("valid regex");
    let mut out = Vec::new();
    let mut fence = false;
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let prose: String = line.split('`').step_by(2).collect::<Vec<_>>().join(" ");
        for c in re.captures_iter(&prose) {
            let t = &c[1];
            if t.starts_with('#') || t.contains("://") || t.starts_with("mailto:") || t.starts_with('<') {
                continue;
            }
            out.push((i + 1, t.split('#').next().unwrap_or(t).to_string()));
        }
    }
    out
}

/// `a/b/../c` as `a/c`, without touching the filesystem; `None` when it climbs above the start.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Every file under `dir`, sorted, relative to `dir`.
fn files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                walk(base, &path, out)?;
            } else {
                out.push(path.strip_prefix(base).unwrap_or(&path).to_path_buf());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if dir.is_dir() {
        walk(dir, dir, &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// Runs every check on the tree at `root`.
pub fn lint(root: &Path) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let read = |rel: &str, out: &mut Vec<Finding>| -> Option<String> {
        match std::fs::read_to_string(root.join(rel)) {
            Ok(t) => Some(t),
            Err(_) => {
                out.push(Finding {
                    code: "onboarding_file_missing",
                    file: rel.into(),
                    message: "missing".into(),
                });
                None
            }
        }
    };
    let Some(spec) = read(SPEC, &mut out) else {
        return Ok(out);
    };
    let facts = spec_facts(&spec);
    let overlay = root.join(OVERLAY);
    let cli = CliSurface::read(root)?;
    let pending: BTreeSet<String> = read_list(&root.join(PENDING))?.into_iter().collect();
    let forbidden = read_list(&root.join(FORBIDDEN))?;

    for entry in &pending {
        if cli.has(entry) {
            out.push(Finding {
                code: "pending_command_exists",
                file: PENDING.into(),
                message: format!("`regia {entry}` exists now: remove it from the pending list"),
            });
        }
    }

    // Spec self-consistency: Appendix A uses the labels it defines.
    check_labels(
        &facts,
        &format!("{SPEC} (Appendix A)"),
        &facts.appendix_a,
        true,
        &mut out,
    );
    check_commands(
        &cli,
        &pending,
        &format!("{SPEC} (Appendix A)"),
        &facts.appendix_a,
        &mut out,
    );

    let agents_rel = format!("{OVERLAY}/AGENTS.md");
    let agents = read(&agents_rel, &mut out).unwrap_or_default();
    if !agents.is_empty() {
        if agents.lines().next() != Some(AGENTS_MARKER) {
            out.push(Finding {
                code: "agents_md_marker_missing",
                file: agents_rel.clone(),
                message: format!("the first line must be `{AGENTS_MARKER}`"),
            });
        }
        let lines = agents.lines().count();
        if lines > AGENTS_MAX_LINES {
            out.push(Finding {
                code: "agents_md_too_long",
                file: agents_rel.clone(),
                message: format!("{lines} lines, at most {AGENTS_MAX_LINES}"),
            });
        }
        let (want, have) = (h2s(&facts.appendix_a), h2s(&agents));
        if want != have {
            out.push(Finding {
                code: "agents_md_sections",
                file: agents_rel.clone(),
                message: format!("sections {have:?}, the contract's Appendix A has {want:?}"),
            });
        }
        for template in [question_template(facts.total), CONSENT_TEMPLATE.to_string()] {
            if !agents.contains(&template) {
                out.push(Finding {
                    code: "label_template_missing",
                    file: agents_rel.clone(),
                    message: format!("the rules must state the label `{template}`"),
                });
            }
        }
        check_labels(&facts, &agents_rel, &agents, true, &mut out);
        let have = prerequisite_rows(&agents);
        if have != facts.prerequisites {
            out.push(Finding {
                code: "prerequisites_differ",
                file: agents_rel.clone(),
                message: format!(
                    "the `| OS |` table names {have:?}, the contract § 6.1 names {:?}",
                    facts.prerequisites
                ),
            });
        }
        if let Some(install) = read(BOOK_INSTALL, &mut out) {
            let distros: Vec<String> = rows_after(&install, &["Distribution"])
                .into_iter()
                .filter_map(|r| r.first().cloned())
                .collect();
            let linux = rows_after(&agents, &["OS"])
                .into_iter()
                .find(|r| r.first().map(String::as_str) == Some("Linux"))
                .and_then(|r| r.get(1).cloned())
                .unwrap_or_default();
            for name in ["Debian", "Fedora", "Arch"] {
                if linux.contains(name) && !distros.iter().any(|d| d.contains(name)) {
                    out.push(Finding {
                        code: "install_row_missing",
                        file: BOOK_INSTALL.into(),
                        message: format!(
                            "the user AGENTS.md names {name}, the `| Distribution |` table has no row"
                        ),
                    });
                }
            }
        }
    }

    let claude_rel = format!("{OVERLAY}/CLAUDE.md");
    if let Some(claude) = read(&claude_rel, &mut out)
        && claude.trim_end() != facts.claude_md.trim_end()
    {
        out.push(Finding {
            code: "claude_md_differs",
            file: claude_rel,
            message: "must equal the contract § 4.2 exactly".into(),
        });
    }

    // Skills: sources outside the overlay, so a session on this repository never loads them as its own.
    if overlay.join(".claude").exists() {
        out.push(Finding {
            code: "skill_in_overlay",
            file: format!("{OVERLAY}/.claude"),
            message: format!(
                "a `.claude/` folder in the overlay loads as live skills in sessions on this repository: keep the skills in {SKILLS}/"
            ),
        });
    }
    let skills_dir = root.join(SKILLS);
    let present: BTreeSet<String> = std::fs::read_dir(&skills_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    let agent_rules = rules(&agents);
    for name in &facts.skills {
        if !present.contains(name) {
            out.push(Finding {
                code: "skill_missing",
                file: format!("{SKILLS}/{name}/SKILL.md"),
                message: "the contract § 5.1 lists this skill".into(),
            });
        }
    }
    for folder in &present {
        let rel = format!("{SKILLS}/{folder}/SKILL.md");
        if !facts.skills.contains(folder) {
            out.push(Finding {
                code: "skill_unknown",
                file: rel.clone(),
                message: "not a skill of the contract § 5.1".into(),
            });
        }
        let Some(text) = read(&rel, &mut out) else {
            continue;
        };
        let Some((meta, body)) = front_matter(&text) else {
            out.push(Finding {
                code: "skill_front_matter",
                file: rel,
                message: "no `---` front matter".into(),
            });
            continue;
        };
        if meta.get("name") != Some(folder) {
            out.push(Finding {
                code: "skill_front_matter",
                file: rel.clone(),
                message: format!("`name` is {:?}, the folder is `{folder}`", meta.get("name")),
            });
        }
        if meta.get("description").is_none_or(|d| d.is_empty()) {
            out.push(Finding {
                code: "skill_front_matter",
                file: rel.clone(),
                message: "empty `description`".into(),
            });
        }
        let lines = body.lines().count();
        if lines > SKILL_MAX_LINES {
            out.push(Finding {
                code: "skill_too_long",
                file: rel.clone(),
                message: format!("body has {lines} lines, at most {SKILL_MAX_LINES}"),
            });
        }
        if !body.contains("AGENTS.md") {
            out.push(Finding {
                code: "skill_no_agents_link",
                file: rel.clone(),
                message: "the body must link the AGENTS.md section it implements".into(),
            });
        }
        let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
        for rule in &agent_rules {
            if rule.len() > 40 && flat.contains(rule.as_str()) {
                out.push(Finding {
                    code: "skill_duplicates_rule",
                    file: rel.clone(),
                    message: format!("repeats an AGENTS.md rule (link it instead): {rule}"),
                });
            }
        }
        check_labels(&facts, &rel, &body, folder == SETUP_SKILL, &mut out);
    }

    // The book pages.
    if let Some(page) = read(BOOK_ONBOARDING, &mut out) {
        check_labels(&facts, BOOK_ONBOARDING, &page, true, &mut out);
        check_commands(&cli, &pending, BOOK_ONBOARDING, &page, &mut out);
    }
    if let Some(page) = read(BOOK_ASSISTANT, &mut out) {
        check_labels(&facts, BOOK_ASSISTANT, &page, false, &mut out);
        check_commands(&cli, &pending, BOOK_ASSISTANT, &page, &mut out);
    }

    // The MCP template.
    let mcp_rel = format!("{OVERLAY}/dist/mcp/regia.mcp.json.example");
    if let Some(text) = read(&mcp_rel, &mut out) {
        let ok = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .is_some_and(|v| {
                v["mcpServers"]["regia"]["command"].is_string()
                    && v["mcpServers"]["regia"]["args"]
                        .as_array()
                        .is_some_and(|a| a.first().and_then(|x| x.as_str()) == Some("mcp"))
            });
        if !ok {
            out.push(Finding {
                code: "mcp_template_invalid",
                file: mcp_rel,
                message: "must be JSON with `mcpServers.regia.command` and `args` starting with `mcp` (the contract)".into(),
            });
        }
    }

    // Every overlay and skill text file: commands, links, forbidden terms. A link resolves from where the
    // export puts the file (the overlay at the root, the skills under `.claude/skills/`).
    let sources = [
        (overlay.clone(), OVERLAY, PathBuf::new()),
        (skills_dir.clone(), SKILLS, PathBuf::from(SKILLS_PUBLISHED)),
    ];
    for (dir_abs, prefix, published) in &sources {
        for rel in files_under(dir_abs)? {
            let name = rel.to_string_lossy().replace('\\', "/");
            if prefix == &OVERLAY
                && (name == crate::llms_txt::LLMS_TXT || name == crate::llms_txt::LLMS_FULL_TXT)
            {
                continue;
            }
            let file = format!("{prefix}/{name}");
            let Ok(text) = std::fs::read_to_string(dir_abs.join(&rel)) else {
                continue;
            };
            if name.ends_with(".md") {
                check_commands(&cli, &pending, &file, &text, &mut out);
                let dir = published.join(rel.parent().unwrap_or(Path::new("")));
                for (line, target) in relative_links(&text) {
                    // Lexically: `dist/mcp/../../book` must not need `dist/mcp` to exist in the private root.
                    let path = normalize(&dir.join(&target));
                    if !path
                        .as_ref()
                        .is_some_and(|p| overlay.join(p).exists() || root.join(p).exists())
                    {
                        out.push(Finding {
                            code: "link_missing",
                            file: file.clone(),
                            message: format!(
                                "line {line}: `{target}` resolves neither in the overlay nor the repository"
                            ),
                        });
                    }
                }
            }
            let lower = text.to_lowercase();
            for term in &forbidden {
                if has_term(&lower, term) {
                    out.push(Finding {
                        code: "forbidden_term",
                        file: file.clone(),
                        message: format!("contains a term from {FORBIDDEN} (`{term}`)"),
                    });
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Runs `cargo xtask onboarding-lint`.
pub fn run(root: &Path) -> Result<ExitCode> {
    let findings = lint(root)?;
    if findings.is_empty() {
        println!("onboarding-lint: ok");
        return Ok(ExitCode::SUCCESS);
    }
    for f in &findings {
        eprintln!("onboarding-lint: {f}");
    }
    eprintln!("onboarding-lint: {} finding(s)", findings.len());
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spec with the sections the lint reads.
    const SPEC_TEXT: &str = "# 125\n\n### 4.2 `CLAUDE.md`\n\n```markdown\n@AGENTS.md\n\nUse the skills.\n```\n\n\
        ### 5.1 Skills\n\n| Skill | Description (trigger) | Body |\n|---|---|---|\n| `regia-setup` | Set up. | x |\n\
        | `regia-troubleshoot` | Fix. | y |\n\n### 6.1 Bootstrap\n\n| OS | Prerequisites |\n|---|---|\n\
        | Linux | `build-essential` (Debian), `base-devel` (Arch) |\n| macOS | `xcode-select --install` |\n\n\
        ### 6.3 `regia setup`\n\n| Id | Does | Consent class |\n|---|---|---|\n| `licence` | a | b |\n\
        | `rust_toolchain` | a | b |\n| `android_sdk`, `jdk` | a | b |\n\n## 7. The question script\n\n\
        | # | Id | Ask |\n|---|---|---|\n| — | `intro` | x |\n| Q1 | `machine` | x |\n| Q2 | `licence` | x |\n\
        | Q2b | `client` | x |\n| Q3 | `mode` | x |\n\n## Appendix A. Draft\n\n````markdown\n\
        <!-- regia:user-agents-md -->\n# Regia\n\nIntro.\n\n## Rules\n\n1. Label `(Question N of 3 · <id>)`.\n\n\
        ## Setting up\n\n(Question 1 of 3 · machine) (Question 2 of 3 · licence) (Question 2b · client)\n\
        (Question 3 of 3 · mode)\n````\n\n## Appendix B\n";

    const AGENTS: &str = "<!-- regia:user-agents-md -->\n# Regia\n\nRegia is an engine.\n\n## Rules\n\n\
        1. Ask the setup questions one at a time, labeled `(Question N of 3 · <id>)`, and consents\n   \
        `(Consent · <step id>)`.\n2. Never accept the license for the user; the user runs `regia licence accept`.\n\n\
        ## Setting up\n\n| OS | Prerequisites |\n|---|---|\n| Linux | `build-essential` (Debian), `base-devel` (Arch) |\n\
        | macOS | `xcode-select --install` |\n\n(Question 1 of 3 · machine) `regia doctor --json`\n\
        (Question 2 of 3 · licence) (Consent · rust_toolchain)\n(Question 2b · client)\n(Question 3 of 3 · mode) \
        `regia mcp token` and `cargo run -p regia_cli -- setup --plan`.\n";

    const SKILL_SETUP: &str = "---\nname: regia-setup\ndescription: Set up Regia.\n---\n\n\
        Follow AGENTS.md § Setting up. (Question 1 of 3 · machine) (Question 2 of 3 · licence)\n\
        (Question 2b · client) (Question 3 of 3 · mode) `regia setup run`\n";

    const SKILL_FIX: &str = "---\nname: regia-troubleshoot\ndescription: Fix a failed install.\n---\n\n\
        See AGENTS.md. Run `regia doctor --json` and `regia agent call agent.hello`.\n";

    const ONBOARDING: &str = "# Onboarding\n\n(Question 1 of 3 · machine) (Question 2 of 3 · licence) \
        (Question 2b · client) (Question 3 of 3 · mode)\n\n```console\n$ regia setup status --json\n```\n";

    const MAIN_RS: &str = "enum AgentCommand { Call(CallArgs) }\nenum CliCommand {\n    #[command(subcommand)]\n    \
        Agent(AgentCommand),\n    Doctor(DoctorArgs),\n    Mcp(McpArgs),\n    New(NewArgs),\n}\n\
        fn attach(c: Command) { c.find_subcommand_mut(\"mcp\"); Command::new(\"token\"); }\n";

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, text).expect("write");
    }

    /// A tree that passes every check.
    fn fixture() -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("onboarding_lint");
        write(&root, SPEC, SPEC_TEXT);
        write(&root, "crates/regia_cli/src/main.rs", MAIN_RS);
        write(
            &root,
            PENDING,
            "# commands the docs name before they land\nsetup\nlicence\n",
        );
        write(&root, FORBIDDEN, "# private\nfarm\nlane\nhost-a\n10.1.2.\n");
        write(&root, &format!("{OVERLAY}/AGENTS.md"), AGENTS);
        write(
            &root,
            &format!("{OVERLAY}/CLAUDE.md"),
            "@AGENTS.md\n\nUse the skills.\n",
        );
        write(
            &root,
            &format!("{OVERLAY}/README.md"),
            "# Regia\n\nRead [AGENTS](AGENTS.md) and [install](book/start/install.md).\n",
        );
        write(&root, &format!("{SKILLS}/regia-setup/SKILL.md"), SKILL_SETUP);
        write(&root, &format!("{SKILLS}/regia-troubleshoot/SKILL.md"), SKILL_FIX);
        write(
            &root,
            &format!("{OVERLAY}/dist/mcp/regia.mcp.json.example"),
            "{\"mcpServers\": {\"regia\": {\"command\": \"/abs/regia\", \"args\": [\"mcp\", \"--project\", \"/abs/p\"]}}}\n",
        );
        write(&root, BOOK_ONBOARDING, ONBOARDING);
        write(
            &root,
            BOOK_ASSISTANT,
            "# Assistant setup\n\nSay `regia new \"My Game\"`.\n",
        );
        write(
            &root,
            BOOK_INSTALL,
            "# Install\n\n| Distribution | Command |\n|---|---|\n| Debian, Ubuntu | x |\n| Arch | y |\n",
        );
        root
    }

    fn codes(root: &Path) -> Vec<&'static str> {
        lint(root).expect("lint").into_iter().map(|f| f.code).collect()
    }

    #[test]
    fn the_spec_facts_are_read() {
        let f = spec_facts(SPEC_TEXT);
        assert_eq!(
            f.questions,
            [
                ("1", "machine"),
                ("2", "licence"),
                ("2b", "client"),
                ("3", "mode")
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
        assert_eq!(f.total, 3);
        assert_eq!(
            f.steps.iter().map(String::as_str).collect::<Vec<_>>(),
            ["android_sdk", "jdk", "licence", "rust_toolchain"]
        );
        assert_eq!(f.skills, ["regia-setup", "regia-troubleshoot"]);
        assert_eq!(f.claude_md, "@AGENTS.md\n\nUse the skills.\n");
        assert!(f.appendix_a.starts_with("<!-- regia:user-agents-md -->\n"));
        assert_eq!(f.prerequisites.len(), 2);
    }

    #[test]
    fn a_consistent_tree_passes() {
        let root = fixture();
        let findings = lint(&root).expect("lint");
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn a_planted_question_id_mismatch_fails() {
        let root = fixture();
        let rel = format!("{OVERLAY}/AGENTS.md");
        write(
            &root,
            &rel,
            &AGENTS.replace("(Question 2 of 3 · licence)", "(Question 2 of 3 · install_from)"),
        );
        let c = codes(&root);
        assert!(c.contains(&"question_id_mismatch"), "{c:?}");
        assert!(c.contains(&"question_missing"), "{c:?}");
        // A wrong total, and an out-of-order book page.
        let root = fixture();
        write(
            &root,
            BOOK_ONBOARDING,
            &ONBOARDING.replace("2 of 3 · licence", "2 of 7 · licence"),
        );
        assert_eq!(codes(&root), ["question_total_mismatch"]);
        let root = fixture();
        write(
            &root,
            BOOK_ONBOARDING,
            "(Question 2 of 3 · licence) (Question 1 of 3 · machine) (Question 2b · client) (Question 3 of 3 · mode)\n",
        );
        assert_eq!(codes(&root), ["question_order"]);
    }

    #[test]
    fn a_planted_unknown_subcommand_fails() {
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/README.md"),
            "# Regia\n\nRun `regia frobnicate --all`.\n",
        );
        assert_eq!(codes(&root), ["command_unknown"]);
        // A nested path under a command with children is checked too; prose is not code.
        let root = fixture();
        write(
            &root,
            BOOK_ASSISTANT,
            "# A\n\nThe regia frobnicate tool.\n\n```\nregia agent poke x\n```\n",
        );
        let f = lint(&root).expect("lint");
        assert_eq!(f.len(), 1, "{f:#?}");
        assert!(f[0].message.contains("`regia agent poke`"), "{f:#?}");
    }

    #[test]
    fn a_pending_command_that_exists_fails() {
        let root = fixture();
        write(&root, PENDING, "setup\nlicence\ndoctor\nmcp token\n");
        let f = lint(&root).expect("lint");
        let c: Vec<_> = f.iter().map(|f| f.code).collect();
        assert_eq!(c, ["pending_command_exists", "pending_command_exists"], "{f:#?}");
    }

    #[test]
    fn a_201_line_agents_md_fails() {
        let root = fixture();
        let long = format!(
            "{AGENTS}{}",
            "\n".repeat(AGENTS_MAX_LINES + 1 - AGENTS.lines().count())
        );
        assert_eq!(long.lines().count(), AGENTS_MAX_LINES + 1);
        write(&root, &format!("{OVERLAY}/AGENTS.md"), &long);
        assert_eq!(codes(&root), ["agents_md_too_long"]);
        let ok = format!(
            "{AGENTS}{}",
            "\n".repeat(AGENTS_MAX_LINES - AGENTS.lines().count())
        );
        write(&root, &format!("{OVERLAY}/AGENTS.md"), &ok);
        assert!(codes(&root).is_empty());
    }

    #[test]
    fn a_skill_name_differing_from_its_folder_fails() {
        let root = fixture();
        write(
            &root,
            &format!("{SKILLS}/regia-troubleshoot/SKILL.md"),
            &SKILL_FIX.replace("name: regia-troubleshoot", "name: regia-fix"),
        );
        assert_eq!(codes(&root), ["skill_front_matter"]);
        // An over-long body, a missing description and a copied rule fail too.
        let root = fixture();
        let body = format!("{SKILL_FIX}{}", "line\n".repeat(SKILL_MAX_LINES));
        write(&root, &format!("{SKILLS}/regia-troubleshoot/SKILL.md"), &body);
        assert_eq!(codes(&root), ["skill_too_long"]);
        let root = fixture();
        let copied = format!(
            "{}\nNever accept the license for the user; the user runs `regia licence accept`.\n",
            SKILL_FIX.replace("description: Fix a failed install.", "description:")
        );
        write(&root, &format!("{SKILLS}/regia-troubleshoot/SKILL.md"), &copied);
        assert_eq!(codes(&root), ["skill_duplicates_rule", "skill_front_matter"]);
    }

    #[test]
    fn a_planted_forbidden_term_fails() {
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/README.md"),
            "# Regia\n\nBuilt on our Farm at 10.1.2.3.\n",
        );
        let f = lint(&root).expect("lint");
        assert_eq!(f.len(), 2, "{f:#?}");
        assert!(f.iter().all(|f| f.code == "forbidden_term"));
        // Whole words only for word terms: `planet`, `farmhouse` and `lanes` are fine.
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/README.md"),
            "# Regia\n\nA planet near a farmhouse with lanes.\n",
        );
        assert!(codes(&root).is_empty());
    }

    #[test]
    fn shape_checks_catch_marker_sections_claude_md_links_and_mcp() {
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/AGENTS.md"),
            &AGENTS.replacen("<!-- regia:user-agents-md -->\n", "", 1),
        );
        assert_eq!(codes(&root), ["agents_md_marker_missing"]);
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/AGENTS.md"),
            &AGENTS.replace("## Setting up", "## Setup"),
        );
        assert_eq!(codes(&root), ["agents_md_sections"]);
        let root = fixture();
        write(&root, &format!("{OVERLAY}/CLAUDE.md"), "@AGENTS.md\n");
        assert_eq!(codes(&root), ["claude_md_differs"]);
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/README.md"),
            "# Regia\n\n[gone](book/nowhere.md)\n",
        );
        assert_eq!(codes(&root), ["link_missing"]);
        // A nested overlay file links up into the root book: resolved lexically.
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/dist/mcp/README.md"),
            "# MCP\n\n[install](../../book/start/install.md)\n",
        );
        assert!(codes(&root).is_empty());
        write(
            &root,
            &format!("{OVERLAY}/dist/mcp/README.md"),
            "# MCP\n\n[out](../../../../x.md)\n",
        );
        assert_eq!(codes(&root), ["link_missing"]);
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/dist/mcp/regia.mcp.json.example"),
            "{\"mcpServers\": {}}\n",
        );
        assert_eq!(codes(&root), ["mcp_template_invalid"]);
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/AGENTS.md"),
            &AGENTS.replace("(Consent · rust_toolchain)", "(Consent · rust)"),
        );
        assert_eq!(codes(&root), ["consent_step_unknown"]);
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/AGENTS.md"),
            &AGENTS.replace("`base-devel` (Arch)", "`base-devel` `git` (Arch)"),
        );
        assert_eq!(codes(&root), ["prerequisites_differ"]);
        let root = fixture();
        write(
            &root,
            BOOK_INSTALL,
            "# Install\n\n| Distribution | Command |\n|---|---|\n| Arch | y |\n",
        );
        assert_eq!(codes(&root), ["install_row_missing"]);
        let root = fixture();
        std::fs::create_dir_all(root.join(format!("{SKILLS}/regia-extra"))).expect("mkdir");
        write(
            &root,
            &format!("{SKILLS}/regia-extra/SKILL.md"),
            "---\nname: regia-extra\ndescription: x\n---\nAGENTS.md\n",
        );
        assert_eq!(codes(&root), ["skill_unknown"]);
    }

    #[test]
    fn a_claude_folder_in_the_overlay_fails() {
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/.claude/skills/regia-setup/SKILL.md"),
            SKILL_SETUP,
        );
        assert_eq!(codes(&root), ["skill_in_overlay"]);
    }

    #[test]
    fn a_skill_link_resolves_from_its_published_folder() {
        // Published at `.claude/skills/regia-troubleshoot/`: three levels up is the snapshot root.
        let root = fixture();
        let linked = format!("{SKILL_FIX}\n[install](../../../book/start/install.md)\n");
        write(&root, &format!("{SKILLS}/regia-troubleshoot/SKILL.md"), &linked);
        assert!(codes(&root).is_empty(), "{:?}", codes(&root));
        // Resolved from the source folder instead, it would climb out of the repository.
        let wrong = format!("{SKILL_FIX}\n[install](../../book/start/install.md)\n");
        write(&root, &format!("{SKILLS}/regia-troubleshoot/SKILL.md"), &wrong);
        assert_eq!(codes(&root), ["link_missing"]);
        // Skills are scanned for forbidden terms and commands like overlay files.
        let root = fixture();
        let bad = format!("{SKILL_FIX}\nAsk the farm; run `regia frobnicate`.\n");
        write(&root, &format!("{SKILLS}/regia-troubleshoot/SKILL.md"), &bad);
        assert_eq!(codes(&root), ["command_unknown", "forbidden_term"]);
    }

    #[test]
    fn the_label_template_follows_the_question_count() {
        assert_eq!(question_template(6), "(Question N of 6 · <id>)");
        let root = fixture();
        write(
            &root,
            &format!("{OVERLAY}/AGENTS.md"),
            &AGENTS.replace("(Question N of 3 · <id>)", "(Question N of 7 · <id>)"),
        );
        assert_eq!(codes(&root), ["label_template_missing"]);
    }

    #[test]
    fn command_mentions_read_code_only() {
        let text = "Prose regia frobnicate.\n`regia doctor --json` and `<engine>/bin/regia setup run`.\n\
                    ```sh\nregia mcp token --project .\ncargo run --release -p regia_cli -- setup --plan\n```\n\
                    `regia-setup` and `regia_cli` and the `regia` server.\n";
        let m: Vec<(usize, Vec<String>)> = command_mentions(text);
        let words: Vec<String> = m.iter().map(|(_, w)| w.join(" ")).collect();
        assert_eq!(words, ["doctor", "setup run", "mcp token", "setup"]);
    }
}
