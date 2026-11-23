// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask release-notes`: build one release note from the fragments a revision range adds.
//!
//! ```text
//! cargo xtask release-notes --from v0.2.13 --to HEAD [--tag v0.2.14] [--out FILE] [--include-modified]
//! cargo xtask release-notes --from v0.2.13 --to HEAD --tag v0.2.14 --archive
//! cargo xtask release-notes --check [--write-baseline]
//! ```
//!
//! # What it does
//!
//! - **Selection.** `git diff --name-status --no-renames <from> <to> -- docs/releases` lists the fragments the
//!   range adds (status `A`; `M` too with `--include-modified`). A fragment is a root file `v<digits>…-<lane>.md`
//!   that is not a release note or a draft, a file in `docs/releases/unreleased/`, or a file in
//!   `docs/releases/fragments/<tag>/` (ADR-2312, ADR-2767). A fragment that the range adds in `fragments/` while it
//!   deletes the same file name elsewhere is a move, not a new fragment.
//! - **Parsing.** Each fragment is read at `<to>` (`git show`). `## New`, `## Changed`, `## Fixed`, `## Upgrade`
//!   and `## Known issues` (and any other `##` section, such as `## Golden changes`) hold *items*: one top-level
//!   list entry with its continuation lines, or one paragraph. A `###` heading inside a section names the *area*
//!   of the items under it; items before any `###` are in `General`. Code fences are never split.
//! - **Output.** One note: a draft banner, a placeholder for the owner-voice summary, then the sections in a fixed
//!   order, each grouped by area. Every item keeps its text verbatim and ends with `<!-- src: <fragment>.md -->`.
//!   Identical items in several fragments are merged with all their sources. Items in **Upgrade** and
//!   **Known issues** that only say there is nothing to report (a closed vocabulary such as `No API changes.`) are
//!   collapsed into one `<!-- none: … -->` comment per section. The output has no timestamps and is the same for
//!   the same input.
//! - **Counts.** The note's item count is computed from the text it wrote (the `src` and `none` comments) and
//!   compared with the items parsed from the fragments; every Upgrade and Known issues item must also appear
//!   verbatim in the note. A mismatch prints an error and fails the command.
//! - **`--check`** lints the fragments in the working tree (required headings, a `# ` title, an empty Upgrade
//!   claim next to a `docs/api-snapshots/*.txt` change in the commit that brought the fragment in, and the naming
//!   rule). Findings listed in the baseline (`xtask/release-notes-baseline.txt`, one `file<TAB>rule` per line)
//!   are old and only counted; any other finding is an error. `--write-baseline` rewrites the baseline from the
//!   current findings (the lead does that once, never to hide a new fragment).
//! - **`--archive`** (with `--tag`) moves the selected fragments from the working tree into
//!   `docs/releases/fragments/<tag>/`. The lead runs it once between gates; `adr-index` reads that folder.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;

use crate::adr_index::{RELEASES, release_version};

/// Baseline of known lint findings in old fragments, relative to the workspace root.
pub const BASELINE: &str = "xtask/release-notes-baseline.txt";
/// The fragment prefix the sprint lane template still uses (ADR-2312 keeps it valid until the template changes).
/// ADR-7624 adds the `v<major>.<minor>.<patch>-` prefix from `v0.5.1` on ([`FIRST_VERSIONED`]).
pub const LEGACY_PREFIX: &str = "v0.1.28-";

/// The first release whose fragments may carry their version in the name (ADR-7624, ADR-7684): a root fragment
/// `v<major>.<minor>.<patch>-<lane>.md` is valid from `v0.5.1` on. An older version prefix other than
/// [`LEGACY_PREFIX`] is a stale lane template (`v0.1.21-open-kit.md`) and stays a finding.
pub const FIRST_VERSIONED: (u32, u32, u32) = (0, 5, 1);

/// `name` split into its `v<major>.<minor>.<patch>-` release prefix and the rest, if it has such a prefix
/// (ADR-7624, such as `v0.5.1-`). Each of the three parts is one or more ASCII digits that fit in a `u32`.
pub fn split_version_prefix(name: &str) -> Option<((u32, u32, u32), &str)> {
    let rest = name.strip_prefix('v')?;
    let mut parts = rest.splitn(3, '.');
    let (major, minor, tail) = (parts.next()?, parts.next()?, parts.next()?);
    let (patch, lane) = tail.split_once('-')?;
    let number = |p: &str| {
        if p.bytes().all(|b| b.is_ascii_digit()) {
            p.parse::<u32>().ok()
        } else {
            None
        }
    };
    Some(((number(major)?, number(minor)?, number(patch)?), lane))
}

/// The lane of a root fragment file stem that carries an accepted release prefix: [`LEGACY_PREFIX`] or a
/// version from [`FIRST_VERSIONED`] on.
fn root_fragment_lane(stem: &str) -> Option<&str> {
    if let Some(lane) = stem.strip_prefix(LEGACY_PREFIX) {
        return Some(lane);
    }
    split_version_prefix(stem).and_then(|(version, lane)| (version >= FIRST_VERSIONED).then_some(lane))
}

/// Arguments of `cargo xtask release-notes`.
#[derive(ClapArgs, Debug)]
pub struct Args {
    /// First revision (exclusive): the previous tag.
    #[arg(long)]
    pub from: Option<String>,
    /// Last revision (inclusive); fragments are read at this revision.
    #[arg(long)]
    pub to: Option<String>,
    /// Tag the note is for, such as `v0.2.14`. Names the title and the `--archive` folder.
    #[arg(long)]
    pub tag: Option<String>,
    /// Write the note to this file instead of standard output.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Also take fragments the range modified, not only the ones it added.
    #[arg(long)]
    pub include_modified: bool,
    /// Lint the fragments in the working tree instead of building a note.
    #[arg(long)]
    pub check: bool,
    /// With `--check`: rewrite the baseline from the current findings.
    #[arg(long)]
    pub write_baseline: bool,
    /// Move the selected fragments into `docs/releases/fragments/<tag>/` (needs `--tag`).
    #[arg(long)]
    pub archive: bool,
}

// ---------------------------------------------------------------------------------------------------------------
// Paths and range selection
// ---------------------------------------------------------------------------------------------------------------

/// Whether a repository-relative path is a release fragment (see the module docs).
pub fn is_fragment_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("docs/releases/") else {
        return false;
    };
    if !rest.ends_with(".md") {
        return false;
    }
    if let Some(name) = rest.strip_prefix("unreleased/") {
        return !name.contains('/') && name != "README.md";
    }
    if let Some(inner) = rest.strip_prefix("fragments/") {
        return inner.matches('/').count() == 1 && !inner.ends_with("/README.md");
    }
    if rest.contains('/') || release_version(rest).is_some() || rest.ends_with("-draft.md") {
        return false;
    }
    let mut chars = rest.chars();
    chars.next() == Some('v') && chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// The fragment name used in attributions: the file name without `.md`.
pub fn fragment_name(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".md").unwrap_or(file).to_string()
}

/// Fragment paths a `git diff --name-status --no-renames` output selects: added files, plus modified ones when
/// `include_modified`. Sorted and unique by fragment name. A file added under `fragments/` whose name the same
/// output deletes elsewhere is a move and is skipped.
pub fn select_fragments(name_status: &str, include_modified: bool) -> Vec<String> {
    let mut deleted: BTreeSet<String> = BTreeSet::new();
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    let rows: Vec<(&str, &str)> = name_status
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(s, p)| (s.trim(), p.trim()))
        .collect();
    for (status, path) in &rows {
        if *status == "D" && is_fragment_path(path) {
            deleted.insert(fragment_name(path));
        }
    }
    for (status, path) in rows {
        let wanted = status == "A" || (include_modified && status == "M");
        if !wanted || !is_fragment_path(path) {
            continue;
        }
        let name = fragment_name(path);
        if status == "A" && path.contains("/fragments/") && deleted.contains(&name) {
            continue;
        }
        taken.entry(name).or_insert_with(|| path.to_string());
    }
    taken.into_values().collect()
}

/// The naming-rule problem of a fragment path, if any (ADR-2312, ADR-2767).
pub fn naming_problem(path: &str) -> Option<String> {
    let rest = path.strip_prefix("docs/releases/")?;
    let lane_ok = |lane: &str| {
        !lane.is_empty()
            && lane
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && lane
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    };
    if let Some(name) = rest.strip_prefix("unreleased/") {
        let lane = name.strip_suffix(".md")?;
        if !lane_ok(lane) {
            return Some(format!(
                "lane name `{lane}` must be lowercase ascii, digits, `-`, `_`, `.`"
            ));
        }
        if lane.starts_with('v') && lane.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
            return Some("an unreleased fragment carries no version in its name".into());
        }
        return None;
    }
    if let Some(inner) = rest.strip_prefix("fragments/") {
        let (tag, file) = inner.split_once('/')?;
        if release_version(&format!("{tag}.md")).is_none() {
            return Some(format!("folder `{tag}` is not a release tag"));
        }
        let lane = file.strip_suffix(".md")?;
        return (!lane_ok(split_version_prefix(lane).map_or(lane, |(_, l)| l)))
            .then(|| format!("fragment name `{lane}` is not lowercase ascii"));
    }
    let lane = root_fragment_lane(rest.strip_suffix(".md")?);
    match lane {
        Some(l) if lane_ok(l) => None,
        Some(l) => Some(format!(
            "lane name `{l}` must be lowercase ascii, digits, `-`, `_`, `.`"
        )),
        None => Some(format!(
            // The baseline keys on this text (`xtask/release-notes-baseline.txt`): keep it as it was.
            "a root fragment is named `{LEGACY_PREFIX}<lane>.md`; new ones go to `unreleased/<lane>.md`"
        )),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Fragment parsing
// ---------------------------------------------------------------------------------------------------------------

/// The standard sections of a fragment, in the order a note prints them, then the other headings by name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// `## New`
    New,
    /// `## Changed`
    Changed,
    /// `## Fixed`
    Fixed,
    /// `## Upgrade`
    Upgrade,
    /// `## Known issues`
    Known,
    /// Any other `##` heading, such as `Golden changes` or `Documentation`.
    Other(String),
}

impl Kind {
    fn from_heading(h: &str) -> Kind {
        let l = h.trim().to_ascii_lowercase();
        if l == "new" || l.starts_with("new ") || l.starts_with("added") {
            Kind::New
        } else if l.starts_with("changed") || l == "changes" {
            Kind::Changed
        } else if l.starts_with("fixed") || l == "fixes" {
            Kind::Fixed
        } else if l.starts_with("upgrade") {
            Kind::Upgrade
        } else if l.starts_with("known issue") {
            Kind::Known
        } else {
            Kind::Other(h.trim().to_string())
        }
    }

    fn title(&self) -> String {
        match self {
            Kind::New => "New".into(),
            Kind::Changed => "Changed".into(),
            Kind::Fixed => "Fixed".into(),
            Kind::Upgrade => "Upgrade".into(),
            Kind::Known => "Known issues".into(),
            Kind::Other(s) => s.clone(),
        }
    }
}

/// One item of a fragment section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The `###` heading the item sits under, or `General`.
    pub area: String,
    /// The item's lines verbatim, joined with `\n`, with no trailing blank line.
    pub text: String,
}

/// A parsed fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// File name without `.md`.
    pub name: String,
    /// Text of the first `# ` heading.
    pub title: Option<String>,
    /// Every `##` section in file order; a heading used twice appears twice.
    pub sections: Vec<(Kind, Vec<Entry>)>,
}

impl Fragment {
    /// The items of every section of `kind`.
    pub fn items(&self, kind: &Kind) -> Vec<&Entry> {
        self.sections
            .iter()
            .filter(|(k, _)| k == kind)
            .flat_map(|(_, e)| e.iter())
            .collect()
    }

    /// Whether the fragment has a section of `kind` (even an empty one).
    pub fn has(&self, kind: &Kind) -> bool {
        self.sections.iter().any(|(k, _)| k == kind)
    }

    /// Number of items in all sections.
    pub fn item_count(&self) -> usize {
        self.sections.iter().map(|(_, e)| e.len()).sum()
    }
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

fn is_bullet(line: &str) -> bool {
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
        return true;
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && (line[digits..].starts_with(". ") || line[digits..].starts_with(") "))
}

/// Split the lines of one area block into items.
fn split_items(body: &[String]) -> Vec<String> {
    let mut items: Vec<(Vec<String>, bool)> = Vec::new();
    let mut in_fence = false;
    let mut blanks = 0usize;
    for raw in body {
        let line = raw.trim_end();
        if in_fence {
            if let Some((cur, _)) = items.last_mut() {
                cur.push(line.to_string());
            }
            if is_fence(line) {
                in_fence = false;
            }
            continue;
        }
        if line.is_empty() {
            blanks += 1;
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let bullet = indent == 0 && is_bullet(line);
        let continues = match items.last() {
            None => false,
            Some((_, cur_bullet)) => {
                if bullet {
                    false
                } else if blanks == 0 {
                    true
                } else {
                    indent > 0 && *cur_bullet
                }
            }
        };
        if continues {
            if let Some((cur, _)) = items.last_mut() {
                for _ in 0..blanks {
                    cur.push(String::new());
                }
                cur.push(line.to_string());
            }
        } else {
            items.push((vec![line.to_string()], bullet));
        }
        blanks = 0;
        if is_fence(line) {
            in_fence = true;
        }
    }
    items.into_iter().map(|(l, _)| l.join("\n")).collect()
}

/// Parse a fragment's text. Text before the first `##` (the title and any intro) is not an item.
pub fn parse_fragment(name: &str, text: &str) -> Fragment {
    struct Block {
        kind: Kind,
        area: String,
        lines: Vec<String>,
    }
    let mut title = None;
    let mut blocks: Vec<Block> = Vec::new();
    let mut section_starts: Vec<usize> = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if in_fence {
            if let Some(b) = blocks.last_mut() {
                b.lines.push(line.to_string());
            }
            if is_fence(line) {
                in_fence = false;
            }
            continue;
        }
        if let Some(h) = line.strip_prefix("## ") {
            section_starts.push(blocks.len());
            blocks.push(Block {
                kind: Kind::from_heading(h),
                area: "General".into(),
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(h) = line.strip_prefix("### ")
            && let Some(last) = blocks.last()
        {
            let kind = last.kind.clone();
            blocks.push(Block {
                kind,
                area: h.trim().to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        if title.is_none()
            && let Some(t) = line.strip_prefix("# ")
        {
            title = Some(t.trim().to_string());
            continue;
        }
        if let Some(b) = blocks.last_mut() {
            b.lines.push(line.to_string());
            if is_fence(line) {
                in_fence = true;
            }
        }
    }
    let mut sections: Vec<(Kind, Vec<Entry>)> = Vec::new();
    let mut section_index = 0usize;
    for (i, b) in blocks.iter().enumerate() {
        if section_starts.get(section_index) == Some(&i) {
            sections.push((b.kind.clone(), Vec::new()));
            section_index += 1;
        }
        if let Some((_, entries)) = sections.last_mut() {
            for text in split_items(&b.lines) {
                entries.push(Entry {
                    area: b.area.clone(),
                    text,
                });
            }
        }
    }
    Fragment {
        name: name.to_string(),
        title,
        sections,
    }
}

/// Words an item may use and still be "there is nothing to report". Any other word makes it a real item.
const NOTHING_WORDS: &[&str] = &[
    "none",
    "no",
    "nothing",
    "api",
    "apis",
    "change",
    "changes",
    "changed",
    "host",
    "hosts",
    "visible",
    "host-visible",
    "public",
    "breaking",
    "format",
    "formats",
    "shape",
    "shapes",
    "removed",
    "only",
    "tests",
    "test",
    "docs",
    "doc",
    "documentation",
    "code",
    "and",
    "in",
    "this",
    "lane",
    "to",
    "the",
    "new",
    "items",
    "item",
    "on",
    "a",
    "for",
    "here",
    "required",
    "needed",
    "snapshot",
    "snapshots",
    "surface",
    "protocol",
    "agent",
    "else",
    "stable",
    "crates",
    "crate",
    "any",
    "or",
    "of",
    "is",
    "are",
    "needs",
    "need",
    "upgrade",
    "known",
    "issues",
    "issue",
    "added",
    "at",
    "all",
    "other",
    "than",
    "beyond",
    "internal",
    "behavior",
    "behaviour",
    "feature",
    "features",
    "dependency",
    "dependencies",
    "golden",
    "goldens",
    "unchanged",
    "default",
    "defaults",
];

/// Whether an Upgrade or Known issues item only says there is nothing to report. Items with code spans, digits,
/// colons or any word outside a small closed vocabulary are never empty claims.
pub fn is_empty_claim(text: &str) -> bool {
    let body = text
        .trim_start_matches(|c: char| c == '-' || c == '*' || c == '+' || c.is_whitespace())
        .trim();
    if body.is_empty() || body.len() > 80 || body.contains('`') || body.contains(':') || body.contains('\n') {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    let mut words = lower
        .split(|c: char| c.is_whitespace() || matches!(c, '.' | ',' | ';' | '(' | ')'))
        .filter(|w| !w.is_empty());
    let Some(first) = words.next() else {
        return false;
    };
    if !matches!(first, "none" | "no" | "nothing") {
        return false;
    }
    words.all(|w| NOTHING_WORDS.contains(&w))
}

// ---------------------------------------------------------------------------------------------------------------
// The note
// ---------------------------------------------------------------------------------------------------------------

/// A built note and its counts.
#[derive(Debug)]
pub struct Note {
    /// The markdown text.
    pub text: String,
    /// Items parsed from the fragments (all sections).
    pub expected_items: usize,
    /// Items found in the text written (`src` and `none` attributions).
    pub written_items: usize,
    /// Problems found while checking the note against the fragments; empty when the counts match.
    pub errors: Vec<String>,
}

/// Header facts for [`build_note`].
#[derive(Debug, Default, Clone)]
pub struct NoteInfo {
    /// Previous tag or revision.
    pub from: String,
    /// Revision the fragments were read at.
    pub to: String,
    /// The tag, when known.
    pub tag: Option<String>,
}

struct Merged {
    text: String,
    sources: Vec<String>,
}

/// Build the note for `fragments` (any order; the output sorts them).
pub fn build_note(fragments: &[Fragment], info: &NoteInfo) -> Note {
    let mut frags: Vec<&Fragment> = fragments.iter().collect();
    frags.sort_by(|a, b| a.name.cmp(&b.name));
    frags.dedup_by(|a, b| a.name == b.name);

    // kind -> area key -> (display area, items in source order)
    type Areas = BTreeMap<String, (String, Vec<Merged>)>;
    let mut by_kind: BTreeMap<Kind, Areas> = BTreeMap::new();
    let mut nothing: BTreeMap<Kind, Vec<String>> = BTreeMap::new();
    let mut expected = 0usize;
    for f in &frags {
        for (kind, entries) in &f.sections {
            for e in entries {
                expected += 1;
                if matches!(kind, Kind::Upgrade | Kind::Known) && is_empty_claim(&e.text) {
                    let names = nothing.entry(kind.clone()).or_default();
                    if !names.contains(&f.name) {
                        names.push(f.name.clone());
                    }
                    continue;
                }
                let areas = by_kind.entry(kind.clone()).or_default();
                let (_, items) = areas
                    .entry(e.area.to_ascii_lowercase())
                    .or_insert_with(|| (e.area.clone(), Vec::new()));
                match items.iter_mut().find(|m| m.text == e.text) {
                    Some(m) => {
                        if !m.sources.contains(&f.name) {
                            m.sources.push(f.name.clone());
                        }
                    }
                    None => items.push(Merged {
                        text: e.text.clone(),
                        sources: vec![f.name.clone()],
                    }),
                }
            }
        }
    }

    let tag = info.tag.clone().unwrap_or_else(|| "(untagged)".into());
    let mut out = String::new();
    out.push_str(&format!("# Regia Engine {tag} (draft)\n\n"));
    out.push_str(&format!(
        "> **DRAFT.** Generated by `cargo xtask release-notes --from {} --to {}` from {} fragments. Items keep the\n\
         > fragment text; each ends with the fragment that wrote it. The lead writes the summary below and the\n\
         > release is cut from a reviewed copy of this file.\n\n",
        info.from,
        info.to,
        frags.len()
    ));
    out.push_str(
        "<!-- SUMMARY: owner-voice summary of this release goes here (hand-written placeholder) -->\n\n",
    );
    if !info.from.is_empty() {
        out.push_str(&format!("Builds on {}.\n\n", info.from));
    }
    out.push_str("The fragments: ");
    out.push_str(
        &frags
            .iter()
            .map(|f| format!("`{}`", f.name))
            .collect::<Vec<_>>()
            .join(", "),
    );
    out.push_str(".\n");

    let mut kinds: BTreeSet<Kind> = by_kind.keys().cloned().collect();
    kinds.extend(nothing.keys().cloned());
    for kind in kinds {
        let heading = match &kind {
            Kind::Upgrade => format!("Upgrade: host changes since {}", info.from),
            k => k.title(),
        };
        out.push_str(&format!("\n## {heading}\n"));
        if let Some(areas) = by_kind.get(&kind) {
            let mut keys: Vec<&String> = areas.keys().collect();
            keys.sort_by_key(|k| (k.as_str() != "general", k.to_string()));
            for key in keys {
                let (display, items) = &areas[key];
                out.push_str(&format!("\n### {display}\n\n"));
                for m in items {
                    out.push_str(&attributed(&m.text, &m.sources));
                    out.push('\n');
                }
            }
        }
        if let Some(names) = nothing.get(&kind) {
            out.push_str(&format!("\n<!-- none: {} -->\n", names.join(", ")));
        }
    }

    let known: BTreeSet<String> = frags.iter().map(|f| f.name.clone()).collect();
    let written = count_attributions(&out, &known);
    let mut errors = Vec::new();
    if written != expected {
        errors.push(format!(
            "item count mismatch: {expected} items in the fragments, {written} in the note"
        ));
    }
    for f in &frags {
        for kind in [Kind::Upgrade, Kind::Known] {
            for e in f.items(&kind) {
                if !is_empty_claim(&e.text) && !out.contains(&e.text) {
                    errors.push(format!(
                        "{} {} item is not verbatim in the note: {}",
                        f.name,
                        kind.title(),
                        first_line(&e.text)
                    ));
                }
            }
        }
    }
    Note {
        text: out,
        expected_items: expected,
        written_items: written,
        errors,
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// The item text followed by its attribution comment. A last line that a trailing comment would break (a fence or
/// a table row) gets the comment on its own line.
fn attributed(text: &str, sources: &[String]) -> String {
    let comment = format!("<!-- src: {} -->", sources.join(", "));
    let last = text.lines().last().unwrap_or("");
    if is_fence(last) || last.trim_start().starts_with('|') || last.is_empty() {
        format!("{text}\n{comment}")
    } else {
        format!("{text} {comment}")
    }
}

/// Items in a note: the fragment names (from `names`) in every `src` comment plus those in every `none` comment.
/// Comments an item quotes, such as `<!-- src: ... -->` in prose, name no fragment and count nothing.
pub fn count_attributions(note: &str, names: &BTreeSet<String>) -> usize {
    let mut n = 0;
    let mut rest = note;
    while let Some(start) = rest.find("<!-- ") {
        let after = &rest[start + 5..];
        let Some(end) = after.find(" -->") else { break };
        let body = &after[..end];
        for prefix in ["src: ", "none: "] {
            if let Some(list) = body.strip_prefix(prefix) {
                n += list.split(", ").filter(|s| names.contains(*s)).count();
            }
        }
        rest = &after[end + 4..];
    }
    n
}

// ---------------------------------------------------------------------------------------------------------------
// Lint
// ---------------------------------------------------------------------------------------------------------------

/// Lint findings of one fragment as `rule` strings (`rule` is stable text, used in the baseline).
/// `snapshot_changed` is true when the commit that brought the fragment in changed a `docs/api-snapshots/*.txt`.
pub fn lint_fragment(path: &str, frag: &Fragment, snapshot_changed: bool) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = naming_problem(path) {
        out.push(format!("naming: {p}"));
    }
    if frag.title.is_none() {
        out.push("missing-title".into());
    }
    for (kind, name) in [(Kind::Upgrade, "Upgrade"), (Kind::Known, "Known issues")] {
        if !frag.has(&kind) {
            out.push(format!("missing-heading: ## {name}"));
        }
    }
    if ![Kind::New, Kind::Changed, Kind::Fixed]
        .iter()
        .any(|k| frag.has(k))
    {
        out.push("missing-heading: ## New, ## Changed or ## Fixed".into());
    }
    for (kind, entries) in &frag.sections {
        if entries.is_empty() {
            out.push(format!("empty-section: ## {}", kind.title()));
        }
    }
    if snapshot_changed {
        let real = frag
            .items(&Kind::Upgrade)
            .iter()
            .any(|e| !is_empty_claim(&e.text));
        if !real {
            out.push("empty-upgrade: docs/api-snapshots changed but Upgrade lists nothing".into());
        }
    }
    out
}

/// Parse the baseline text into a set of `(file, rule)` pairs. Blank lines and `#` lines are comments.
pub fn parse_baseline(text: &str) -> BTreeSet<(String, String)> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t'))
        .map(|(f, r)| (f.to_string(), r.to_string()))
        .collect()
}

/// One lint finding: `(fragment file, rule text)`.
pub type Finding = (String, String);

/// Split findings into `(old, new)` against the baseline.
pub fn split_findings(findings: &[Finding], baseline: &BTreeSet<Finding>) -> (Vec<Finding>, Vec<Finding>) {
    findings.iter().cloned().partition(|f| baseline.contains(f))
}

// ---------------------------------------------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------------------------------------------

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every fragment path in the working tree.
fn working_tree_fragments(root: &Path) -> Result<Vec<String>> {
    let base = root.join(RELEASES);
    let mut dirs = vec![base.clone(), base.join("unreleased")];
    if let Ok(rd) = std::fs::read_dir(base.join("fragments")) {
        dirs.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()));
    }
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd {
            let path = entry
                .with_context(|| format!("listing {}", dir.display()))?
                .path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if is_fragment_path(&rel) {
                out.push(rel);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// For each fragment path: whether the commit that brought it onto the first-parent line changed a snapshot.
fn snapshot_changes(root: &Path, paths: &[String]) -> Result<BTreeMap<String, bool>> {
    let log = git(
        root,
        &[
            "log",
            "--first-parent",
            "--diff-merges=first-parent",
            "--name-status",
            "--no-renames",
            "--format=@@%H",
            "--",
            "docs/releases",
            "docs/api-snapshots",
        ],
    )?;
    // Newest commit first. For each path keep the last (oldest) commit that added it.
    let mut adder: BTreeMap<String, usize> = BTreeMap::new();
    let mut snap: Vec<bool> = Vec::new();
    for line in log.lines() {
        if line.starts_with("@@") {
            snap.push(false);
        } else if let Some((s, p)) = line.split_once('\t') {
            let Some(idx) = snap.len().checked_sub(1) else {
                continue;
            };
            if p.starts_with("docs/api-snapshots/") && p.ends_with(".txt") {
                snap[idx] = true;
            }
            if s == "A" {
                adder.insert(p.to_string(), idx);
            }
        }
    }
    Ok(paths
        .iter()
        .map(|p| (p.clone(), adder.get(p).is_some_and(|&i| snap[i])))
        .collect())
}

fn read_baseline(root: &Path) -> BTreeSet<(String, String)> {
    std::fs::read_to_string(root.join(BASELINE))
        .map(|t| parse_baseline(&t))
        .unwrap_or_default()
}

fn run_check(root: &Path, args: &Args) -> Result<ExitCode> {
    let paths = working_tree_fragments(root)?;
    let snaps = match snapshot_changes(root, &paths) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("release-notes: snapshot rule skipped: {e:#}");
            BTreeMap::new()
        }
    };
    let mut findings: Vec<(String, String)> = Vec::new();
    for path in &paths {
        let text = std::fs::read_to_string(root.join(path)).with_context(|| format!("reading {path}"))?;
        let frag = parse_fragment(&fragment_name(path), &text);
        let file = path.trim_start_matches("docs/releases/").to_string();
        for rule in lint_fragment(path, &frag, snaps.get(path).copied().unwrap_or(false)) {
            findings.push((file.clone(), rule));
        }
    }
    findings.sort();
    if args.write_baseline {
        let mut text = String::from(
            "# Known lint findings of old fragments (cargo xtask release-notes --check). file<TAB>rule.\n\
             # A finding in this list is reported as old; any other finding fails the check.\n",
        );
        for (f, r) in &findings {
            text.push_str(&format!("{f}\t{r}\n"));
        }
        std::fs::write(root.join(BASELINE), text).with_context(|| format!("writing {BASELINE}"))?;
        println!("release-notes: wrote {BASELINE} ({} findings)", findings.len());
        return Ok(ExitCode::SUCCESS);
    }
    let (old, new) = split_findings(&findings, &read_baseline(root));
    println!(
        "release-notes: {} fragments checked, {} old findings in the baseline, {} new",
        paths.len(),
        old.len(),
        new.len()
    );
    for (f, r) in &new {
        eprintln!("release-notes: {f}: {r}");
    }
    Ok(if new.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Runs the command against the workspace at `root`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.check {
        return run_check(root, args);
    }
    let (Some(from), Some(to)) = (&args.from, &args.to) else {
        bail!("release-notes needs --from <rev> and --to <rev> (or --check)");
    };
    if args.archive && args.tag.is_none() {
        bail!("--archive needs --tag");
    }
    let status = git(
        root,
        &["diff", "--name-status", "--no-renames", from, to, "--", RELEASES],
    )?;
    let paths = select_fragments(&status, args.include_modified);
    let mut fragments = Vec::new();
    for path in &paths {
        let text = git(root, &["show", &format!("{to}:{path}")])?;
        fragments.push(parse_fragment(&fragment_name(path), &text));
    }
    let mut failed = false;
    for f in &fragments {
        if f.item_count() == 0 {
            eprintln!("release-notes: error: fragment {} has no items", f.name);
            failed = true;
        }
    }
    let note = build_note(
        &fragments,
        &NoteInfo {
            from: from.clone(),
            to: to.clone(),
            tag: args.tag.clone(),
        },
    );
    for e in &note.errors {
        eprintln!("release-notes: error: {e}");
        failed = true;
    }
    eprintln!(
        "release-notes: {} fragments, {} items parsed, {} items in the note",
        fragments.len(),
        note.expected_items,
        note.written_items
    );
    match &args.out {
        Some(out) => {
            let tmp = out.with_extension("md.tmp");
            std::fs::write(&tmp, &note.text).with_context(|| format!("writing {}", tmp.display()))?;
            std::fs::rename(&tmp, out).with_context(|| format!("replacing {}", out.display()))?;
            eprintln!("release-notes: wrote {}", out.display());
        }
        None => print!("{}", note.text),
    }
    if args.archive
        && !failed
        && let Some(tag) = &args.tag
    {
        archive(root, tag, &paths)?;
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Move the fragments that exist in the working tree to `docs/releases/fragments/<tag>/`.
fn archive(root: &Path, tag: &str, paths: &[String]) -> Result<()> {
    if release_version(&format!("{tag}.md")).is_none() {
        bail!("--tag `{tag}` is not a release tag like v0.2.14");
    }
    let dest = root.join(RELEASES).join("fragments").join(tag);
    let mut moved = 0;
    for path in paths {
        let src = root.join(path);
        if !src.is_file() || path.contains("/fragments/") {
            continue;
        }
        std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
        let file = path.rsplit('/').next().unwrap_or(path);
        let target = dest.join(file);
        if target.exists() {
            eprintln!("release-notes: {} exists, left {path} in place", target.display());
            continue;
        }
        std::fs::rename(&src, &target).with_context(|| format!("moving {path}"))?;
        moved += 1;
    }
    eprintln!("release-notes: archived {moved} fragments to docs/releases/fragments/{tag}/");
    Ok(())
}

/// Truth checks for the v0.5.0 candidate note (`docs/releases/v0.5.0.md`), kept next to the
/// fragment tooling that builds it (ADR-5155's refresh sweeps the same sources).
///
/// Every check fails when the note drifts from the tree at the candidate: the Upgrade
/// section's shipped format versions must equal the versions v0.5.0 ships (and, while the
/// note is a draft, the versions the code wrote at the v0.5.0 tag, frozen in
/// `docs/formats/format-hashes-v0.5.0.ron`), breaking changes since
/// the note's base must be listed under Upgrade / Agent protocol, and a Known issue must
/// not cite an ADR that has no heading in `docs/decisions.md`.
///
/// The note is a historical record of what v0.5.0 shipped, so nothing here reads the live
/// format registry: a format bump on the v0.6 line is checked against the v0.6.0 upgrade note
/// by [`v060_note_truth`] instead (ADR-8208).
#[cfg(test)]
mod v050_note_truth {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Workspace root: the xtask manifest sits one level below it.
    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    fn note() -> String {
        fs::read_to_string(root().join("docs/releases/v0.5.0.md")).expect("read v0.5.0.md")
    }

    /// The body of the note's `<hashes> <name>` heading, up to the next heading of the same
    /// or a higher level. The heading must start the line, so `### Agent protocol` does not
    /// match the `#### Agent protocol: ...` subsection of the Added section.
    pub(super) fn section<'a>(note: &'a str, level: usize, name: &str) -> &'a str {
        let marker = format!("{} ", "#".repeat(level));
        let start = note
            .lines()
            .position(|l| {
                l.strip_prefix(&marker).is_some_and(|rest| {
                    rest.strip_prefix(name)
                        .is_some_and(|tail| tail.is_empty() || tail.starts_with([' ', ':']))
                })
            })
            .map(|i| note.lines().take(i).map(|l| l.len() + 1).sum())
            .unwrap_or_else(|| panic!("missing heading {marker}{name}"));
        let rest = &note[start..];
        // The heading line itself counts: starting at 0 cut every section short by the
        // heading's length (found by `v060_note_truth::missing_or_wrong_rows_are_reported`).
        let mut offset = rest.lines().next().map_or(0, |l| l.len() + 1);
        for line in rest.lines().skip(1) {
            let hashes = line.chars().take_while(|&c| c == '#').count();
            if (1..=level).contains(&hashes) && line.chars().nth(hashes) == Some(' ') {
                break;
            }
            offset += line.len() + 1;
        }
        &rest[..offset.min(rest.len())]
    }

    /// A section runs to the next heading of its level or higher and keeps its last line, also
    /// when it is the end of the note.
    #[test]
    fn section_keeps_its_whole_body() {
        let text = "# Title\n\n## Upgrade\n\n### Built\n\n| `ui` | 10 → 11 |\n| `vfx` | 7 → 8 |\n\n## Known issues\n\nNone.\n";
        assert_eq!(
            section(text, 3, "Built"),
            "### Built\n\n| `ui` | 10 → 11 |\n| `vfx` | 7 → 8 |\n\n"
        );
        assert!(section(text, 2, "Upgrade").ends_with("| `vfx` | 7 → 8 |\n\n"));
        assert_eq!(section(text, 2, "Known issues"), "## Known issues\n\nNone.\n");
    }

    /// Every `v<digits>` version on a line, so `v4 → v5` and `v1-v4` both count.
    fn versions_on(line: &str) -> impl Iterator<Item = u32> + '_ {
        line.split('v').filter_map(|tail| {
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                None
            } else {
                digits.parse().ok()
            }
        })
    }

    /// The highest version the Upgrade section claims for a format must be the version the
    /// code wrote at the v0.5.0 tag (rc1-docs-truth-z finding 1: the note said `.ui.ron` v9 /
    /// `.seq.ron` v3 / `.fgiv` v4 while the candidate shipped v10 / v5 / v5). Only the segment
    /// right after the format name counts, so a sibling format on the same line does not leak
    /// its version in. The code side is the frozen v0.5.0 table, not the live constants: the
    /// note records v0.5.0, and a later bump belongs in the v0.6.0 note (ADR-8208,
    /// `v060_note_truth::upgrade_draft_lists_the_code_versions`).
    #[test]
    fn upgrade_lists_shipped_format_versions() {
        let errors = format_version_errors(&note(), &frozen_v050_format_versions());
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// The release-note extension of each checked format and its row name in the format
    /// registry (`docs/formats/format-hashes.ron` and its frozen per-release copies).
    pub(super) const REGISTRY_NAMES: [(&str, &str); 3] =
        [(".ui.ron", "ui"), (".seq.ron", "sequence"), (".fgiv", "baked-gi")];

    /// The frozen format table of a release: `formats: { name: (version, hash) }`, only the
    /// version is read.
    #[derive(serde::Deserialize)]
    pub(super) struct VersionTable {
        /// Every registered format by its registry name.
        pub(super) formats: std::collections::BTreeMap<String, VersionRow>,
    }

    /// One registry row; the schema hash is not read.
    #[derive(serde::Deserialize)]
    pub(super) struct VersionRow {
        /// The format version.
        pub(super) version: u32,
    }

    /// Reads a format table (`docs/formats/format-hashes*.ron`) relative to the workspace root.
    pub(super) fn version_table(path: &str) -> VersionTable {
        let text = fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"));
        ron::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    /// The versions the code wrote at the v0.5.0 tag, by format: the pinned table
    /// `docs/formats/format-hashes-v0.5.0.ron` (equal to `git show v0.5.0:docs/formats/format-hashes.ron`,
    /// never edited after the tag).
    fn frozen_v050_format_versions() -> [(&'static str, u32); 3] {
        let table = version_table("docs/formats/format-hashes-v0.5.0.ron");
        REGISTRY_NAMES.map(|(format, name)| {
            let row = table
                .formats
                .get(name)
                .unwrap_or_else(|| panic!("{name} is not in the frozen v0.5.0 table"));
            (format, row.version)
        })
    }

    /// The pinned literals and the frozen v0.5.0 table agree: both record what v0.5.0 shipped.
    #[test]
    fn frozen_v050_table_matches_shipped_at_v050() {
        assert_eq!(frozen_v050_format_versions(), SHIPPED_AT_V050);
    }

    /// The versions the code writes today, by format.
    pub(super) fn live_format_versions() -> [(&'static str, u32); 3] {
        [
            (".ui.ron", regia_ui::UI_RON_VERSION),
            (".seq.ron", regia_framework::sequencer::SEQUENCE_FORMAT_VERSION),
            (".fgiv", regia_asset::baked_gi::BAKED_GI_VERSION),
        ]
    }

    /// The highest version the Upgrade section claims for `format`, 0 if it names none.
    fn claimed_max(upgrade: &str, format: &str) -> u32 {
        upgrade
            .lines()
            .filter_map(|l| l.find(format).map(|i| &l[i..]))
            .flat_map(|tail| {
                let segment: String = tail.chars().take_while(|&c| c != ',' && c != ';').collect();
                versions_on(&segment).collect::<Vec<u32>>()
            })
            .max()
            .unwrap_or(0)
    }

    /// The format versions v0.5.0 ships, as its Upgrade section must state them. Pinned
    /// literals: a format bump after the tag moves the code constants, never the tagged note
    /// (rev-kimi-28 finding 3).
    pub(super) const SHIPPED_AT_V050: [(&str, u32); 3] = [(".ui.ron", 10), (".seq.ron", 5), (".fgiv", 5)];

    /// Whether the note still carries its DRAFT banner (the first line after the title).
    fn is_draft(note: &str) -> bool {
        note.lines()
            .skip(1)
            .map(str::trim)
            .find(|l| !l.is_empty())
            .is_some_and(|l| l.starts_with("DRAFT"))
    }

    /// Every format whose Upgrade claim differs from the version v0.5.0 ships; while the note
    /// is a draft, also every pinned version the code (`live`) no longer writes, so a pre-tag
    /// bump updates the note and the pin together.
    fn format_version_errors(note: &str, live: &[(&str, u32)]) -> Vec<String> {
        let upgrade = section(note, 2, "Upgrade");
        let draft = is_draft(note);
        let mut errors = Vec::new();
        for (format, pinned) in SHIPPED_AT_V050 {
            let max = claimed_max(upgrade, format);
            if max != pinned {
                errors.push(format!(
                    "{format}: the note's Upgrade section claims up to v{max}, v0.5.0 ships v{pinned}"
                ));
            }
            let code = live.iter().find(|(f, _)| *f == format).map(|&(_, v)| v);
            if draft
                && let Some(code) = code
                && code != pinned
            {
                errors.push(format!(
                    "{format}: the code writes v{code}, the draft note and SHIPPED_AT_V050 say v{pinned}; \
                     a bump before the tag updates both"
                ));
            }
        }
        errors
    }

    /// The note without its DRAFT banner, as it reads once v0.5.0 is tagged.
    fn tagged(note: &str) -> String {
        note.replacen(
            "DRAFT, not a tagged or gate-approved release.",
            "Tagged release.",
            1,
        )
    }

    /// rev-kimi-28 finding 3: the first format bump after the tag (a v0.6.0 bump is
    /// planned) must not fail the gate, because the only "fix" would be rewriting a tagged
    /// note. A tagged note is checked against the versions v0.5.0 shipped, not the live code.
    #[test]
    fn format_versions_survive_a_post_tag_bump() {
        let bumped = live_format_versions().map(|(format, v)| (format, v + 1));
        let errors = format_version_errors(&tagged(&note()), &bumped);
        assert!(
            errors.is_empty(),
            "a post-tag format bump fails the tagged note: {errors:#?}"
        );
    }

    /// While the note is still a draft, a pre-tag bump the note has not caught up with is
    /// still reported (the protection finding 1 asked for stays).
    #[test]
    fn draft_note_still_catches_a_pre_tag_bump() {
        let text = note();
        if !is_draft(&text) {
            return; // tagged: the live-code comparison no longer applies
        }
        let bumped = live_format_versions().map(|(format, v)| (format, v + 1));
        assert_eq!(format_version_errors(&text, &bumped).len(), 3);
    }

    /// A tagged note that misstates a shipped version is still caught.
    #[test]
    fn tagged_note_with_a_wrong_version_fails() {
        let wrong = tagged(&note()).replace("`.ui.ron` v10", "`.ui.ron` v11");
        let errors = format_version_errors(&wrong, &live_format_versions());
        assert!(errors.iter().any(|e| e.starts_with(".ui.ron")), "{errors:#?}");
    }

    /// `editor.hot_reload` was removed and replaced by `agent.hot_reload` (a job) at
    /// integ-v049 (ADR-5993); a strict agent client's method list breaks without a note.
    #[test]
    fn agent_protocol_documents_hot_reload_rename() {
        let text = note();
        let agent = section(&text, 3, "Agent protocol");
        assert!(
            agent.contains("agent.hot_reload"),
            "the Agent protocol section must name the new `agent.hot_reload` method"
        );
        assert!(
            agent.contains("editor.hot_reload"),
            "the Agent protocol section must say `editor.hot_reload` was removed"
        );
    }

    /// Every method the code refuses in a shipping build (`net_dev_only`, the lagcomp
    /// dev gate) must appear in the Changed list of shipping refusals (finding 2c). Bullets
    /// wrap across lines, so continuation lines are joined before matching.
    #[test]
    fn changed_lists_every_shipping_gated_net_method() {
        let flat = note().replace("\n  ", " ");
        for method in [
            "net.set_client_rate",
            "net.grant_relevance",
            "lagcomp.raycast",
            "lagcomp.rewound_shapes",
            "net.set_interp_delay",
            "net.force_correction",
        ] {
            assert!(
                flat.lines().any(|l| l.contains(method) && l.contains("refuse")),
                "{method} is refused in shipping builds but the note lists no such refusal"
            );
        }
    }

    /// `ModsLimits::max_pak_entry_bytes` folded into `max_file_bytes` at integ-v049: a
    /// public field removal that must appear under Upgrade (finding 2b).
    #[test]
    fn upgrade_documents_mods_limits_cap_fold() {
        let text = note();
        let upgrade = section(&text, 2, "Upgrade");
        assert!(
            upgrade.contains("max_pak_entry_bytes") && upgrade.contains("max_file_bytes"),
            "the Upgrade section must document the ModsLimits cap fold into max_file_bytes"
        );
    }

    /// A Known issue must not cite an ADR that has no heading in docs/decisions.md: mark
    /// it `ADR pending` instead (finding 4: ADR-7200 is only promised in the prose of ADR-7298,
    /// the integ-v049-dx12 roll-up). Bullets wrap across lines, so the whole item is checked.
    #[test]
    fn known_issue_adr_cites_resolve() {
        let text = note();
        let known = section(&text, 2, "Known issues");
        let decisions = fs::read_to_string(root().join("docs/decisions.md")).expect("read decisions.md");
        let mut item = String::new();
        let check = |item: &str| {
            if !item.contains("ADR-") {
                return;
            }
            for tail in item.split("ADR-") {
                let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
                if digits.is_empty() {
                    continue;
                }
                let heading = format!("## ADR-{digits}");
                let has_heading = decisions.lines().any(|l| {
                    l.starts_with(&heading)
                        && l.chars()
                            .nth(heading.chars().count())
                            .is_none_or(|c| !c.is_ascii_digit())
                });
                let marked_pending = item.contains("ADR pending");
                assert!(
                    has_heading || marked_pending,
                    "Known issue cites ADR-{digits}, which has no heading in docs/decisions.md: {item}"
                );
            }
        };
        for line in known.lines() {
            if line.starts_with("- ") {
                check(&item);
                item.clear();
            }
            item.push_str(line);
            item.push(' ');
        }
        check(&item);
    }

    /// The "DX12 without DXC crashes in FXC" known issue was fixed by the fable-dx12-stack-l04
    /// lane (ADR-5971): the Known issues section must not carry the crash claim, and the Fixed
    /// section must cite the fix.
    #[test]
    fn fxc_crash_is_fixed_not_a_known_issue() {
        let errors = fxc_errors(&note());
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// The FXC check over any note text.
    fn fxc_errors(note: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if section(note, 2, "Known issues").contains("crashes in FXC") {
            errors.push(
                "the FXC stack-overflow crash is fixed at the candidate (ADR-5971); drop the stale known issue"
                    .to_string(),
            );
        }
        if !section(note, 2, "Fixed").contains("ADR-5971") {
            errors.push("the Fixed section must cite the FXC fix (ADR-5971)".to_string());
        }
        errors
    }

    /// rev-kimi-28 finding 4: a note that cites ADR-5971 only under Known issues (FXC's
    /// slowness) has not cited the fix; the cite must be in the Fixed section.
    #[test]
    fn fxc_fix_cite_outside_fixed_does_not_count() {
        let only_known = "# N\n\n## Fixed\n\n- Something else.\n\n## Known issues\n\n\
                          - FXC is slow; DXC stays recommended. (ADR-5971)\n";
        let errors = fxc_errors(only_known);
        assert!(
            errors.iter().any(|e| e.contains("Fixed section")),
            "an ADR-5971 cite under Known issues passed as the Fixed cite: {errors:#?}"
        );
    }

    /// Each method a shipping build refuses, with the `data.reason` it refuses with
    /// (`crates/regia_agent/src/net_lagcomp.rs` `dev_gate`, `net_interp.rs`, `net_predict.rs`
    /// and the contract table in `contract.rs`).
    const SHIPPING_REFUSALS: [(&str, &str); 6] = [
        ("net.set_client_rate", "net_dev_only"),
        ("net.grant_relevance", "net_dev_only"),
        ("net.set_interp_delay", "net_dev_only"),
        ("net.force_correction", "net_dev_only"),
        ("lagcomp.raycast", "lagcomp_dev_only"),
        ("lagcomp.rewound_shapes", "lagcomp_dev_only"),
    ];

    /// The `errors` list of `method`'s entry in the agent contract table, if it has one.
    fn contract_errors(contract: &str, method: &str) -> Option<Vec<String>> {
        let at = contract.find(&format!("\"{method}\","))?;
        let rest = &contract[at..];
        let open = rest.find("&[")? + 2;
        let close = open + rest[open..].find(']')?;
        Some(
            rest[open..close]
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        )
    }

    /// The `*_dev_only` reason code a refusal bullet names first after `method`.
    fn refusal_code_after<'a>(line: &'a str, method: &str) -> Option<&'a str> {
        let after = &line[line.find(&format!("`{method}`"))? + method.len() + 2..];
        let end = after.find("_dev_only")? + "_dev_only".len();
        let start = after[..end]
            .rfind(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .map_or(0, |i| i + 1);
        Some(&after[start..end])
    }

    /// rev-kimi-28 finding 1: the Changed bullet said `lagcomp.raycast` and
    /// `lagcomp.rewound_shapes` refuse with `net_dev_only`; they refuse with
    /// `lagcomp_dev_only`, and a client matching the reason code is misled. Every refusal
    /// bullet that names a gated method must name that method's own code after it. The table
    /// is checked against the contract's error lists where the contract lists one.
    #[test]
    fn shipping_refusals_name_each_methods_reason_code() {
        let contract =
            fs::read_to_string(root().join("crates/regia_agent/src/contract.rs")).expect("read contract.rs");
        let flat = note().replace("\n  ", " ");
        let mut errors = Vec::new();
        for (method, code) in SHIPPING_REFUSALS {
            if let Some(listed) = contract_errors(&contract, method)
                && listed.iter().any(|e| e.ends_with("_dev_only"))
                && !listed.iter().any(|e| e == code)
            {
                errors.push(format!(
                    "{method}: the contract lists {listed:?}, the table says {code}"
                ));
            }
            let bullets: Vec<&str> = flat
                .lines()
                .filter(|l| l.contains(&format!("`{method}`")) && l.contains("refuse"))
                .collect();
            if bullets.is_empty() {
                errors.push(format!("{method}: no refusal bullet names it"));
            }
            for line in bullets {
                let named = refusal_code_after(line, method);
                if named != Some(code) {
                    errors.push(format!(
                        "{method} refuses with {code}, the note says {named:?}: {line}"
                    ));
                }
            }
        }
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// The summary must not claim the Mobile Low golden misses the seam bar: it passes the
    /// seam/FLIP bars (finding 5). The owner approved its re-bless on 2026-11-11 (Q3; ADR-5993
    /// item 12, blessed in e2dcdad1c), so the summary must say it is re-blessed after the owner's
    /// approval and must no longer say it is held for owner review. Whitespace is flattened first
    /// so a wrapped phrase still matches.
    #[test]
    fn summary_records_mobile_low_golden_rebless() {
        let text = note();
        let summary = section(&text, 2, "What v0.5 delivers");
        let flat = summary.split_whitespace().collect::<Vec<_>>().join(" ");
        for stale in ["Mobile Low golden do not", "Mobile Low golden misses"] {
            assert!(
                !flat.contains(stale),
                "the summary misstates the Mobile Low golden's bar: {stale:?}"
            );
        }
        assert!(
            !flat.contains("held for owner review"),
            "the owner approved the Mobile Low golden re-bless (ADR-5993 item 12); the summary must not say it is held"
        );
        assert!(
            flat.contains("Mobile Low golden is re-blessed after the owner's approval"),
            "the summary must say the Mobile Low golden is re-blessed after the owner's approval"
        );
    }
}

/// Truth checks for the v0.6.0 upgrade note (`docs/releases/v0.6.0-upgrade-draft.md`, ADR-7724),
/// against the live format registry: the live-version checks the v0.5.0 note held before the
/// v0.5.0 tag moved here when the v0.6 line bumped `.ui.ron` past v0.5.0 (ADR-8208).
///
/// Every format whose registry version moved since v0.5.0 needs a row in the note's
/// "Format changes on the v0.6 line (built)" table that states the step (`10 → 11`), and every
/// format v0.5.0 did not have must be named in the Upgrade section.
#[cfg(test)]
mod v060_note_truth {
    use std::fs;
    use std::path::Path;

    use super::v050_note_truth::{
        REGISTRY_NAMES, VersionTable, live_format_versions, section, version_table,
    };

    /// The draft the release lead moves into the v0.6.0 note at the tag.
    const NOTE: &str = "docs/releases/v0.6.0-upgrade-draft.md";

    /// The heading of the table of format changes that have landed.
    const BUILT_TABLE: &str = "Format changes on the v0.6 line (built)";

    fn note() -> String {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        fs::read_to_string(root.join(NOTE)).expect("read the v0.6.0 upgrade draft")
    }

    /// The built-table row of `format`: a table line whose first cell is `` `format` ``.
    fn built_row<'a>(note: &'a str, format: &str) -> Option<&'a str> {
        let cell = format!("| `{format}` |");
        section(note, 3, BUILT_TABLE)
            .lines()
            .find(|l| l.starts_with(&cell))
    }

    /// Every way `note` misstates the registry's move from `shipped` to `live`: a bumped
    /// format with no built row stating `old → new`, and a new format the Upgrade section does
    /// not name.
    fn bump_errors(note: &str, shipped: &VersionTable, live: &VersionTable) -> Vec<String> {
        let upgrade = section(note, 2, "Upgrade");
        let mut errors = Vec::new();
        for (name, now) in &live.formats {
            match shipped.formats.get(name) {
                Some(then) if then.version != now.version => {
                    let step = format!("{} → {}", then.version, now.version);
                    if !built_row(note, name).is_some_and(|row| row.contains(&step)) {
                        errors.push(format!(
                            "{name}: v{} in v0.5.0, v{} now, but the \"{BUILT_TABLE}\" table has no `{name}` row \
                             stating {step}",
                            then.version, now.version
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    if !upgrade.contains(&format!("`{name}`")) {
                        errors.push(format!(
                            "{name}: new since v0.5.0 (v{}), but the Upgrade section does not name `{name}`",
                            now.version
                        ));
                    }
                }
            }
        }
        errors
    }

    /// The note lists every format bump and every new format of the v0.6 line: the frozen
    /// v0.6.0 table (`docs/formats/format-hashes-v0.6.0.ron`, equal to `git show
    /// v0.6.0:docs/formats/format-hashes.ron`) against the frozen v0.5.0 table. Before the
    /// v0.6.0 tag this read the live registry; since the tag the note is a record of what
    /// v0.6.0 shipped, and the live check is the v0.7 upgrade guide's
    /// (`v070_guide_truth`, ADR-10183), so a v0.7 bump does not turn this note red.
    #[test]
    fn upgrade_draft_lists_every_format_change_since_v050() {
        let errors = bump_errors(
            &note(),
            &version_table("docs/formats/format-hashes-v0.5.0.ron"),
            &version_table("docs/formats/format-hashes-v0.6.0.ron"),
        );
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// The live check the v0.5.0 note held while it was a draft (code constant against the
    /// note), now against this note: a format whose code constant moved since v0.5.0 has a
    /// built row stating the step to the version the code writes.
    #[test]
    fn upgrade_draft_lists_the_code_versions() {
        let text = note();
        let shipped = version_table("docs/formats/format-hashes-v0.5.0.ron");
        let mut errors = Vec::new();
        for ((format, code), (_, name)) in live_format_versions().into_iter().zip(REGISTRY_NAMES) {
            let then = shipped.formats.get(name).map_or(0, |row| row.version);
            if code == then {
                continue;
            }
            let step = format!("{then} → {code}");
            if !built_row(&text, name).is_some_and(|row| row.contains(&step)) {
                errors.push(format!(
                    "{format}: the code writes v{code}, v0.5.0 shipped v{then}; the note needs a `{name}` row stating {step}"
                ));
            }
        }
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// The registry rows the checks read are the code constants (so the registry check above is
    /// the live code's truth for these formats, too).
    #[test]
    fn registry_rows_match_the_code_constants() {
        let live = version_table("docs/formats/format-hashes.ron");
        for ((format, code), (_, name)) in live_format_versions().into_iter().zip(REGISTRY_NAMES) {
            let registered = live.formats.get(name).map(|row| row.version);
            assert_eq!(registered, Some(code), "{format}: registry row `{name}`");
        }
    }

    /// The check fires: a note without the `ui` row, a row with the wrong step, and a new
    /// format the note does not name are each reported.
    #[test]
    fn missing_or_wrong_rows_are_reported() {
        let table = |rows: &[(&str, u32)]| VersionTable {
            formats: rows
                .iter()
                .map(|&(name, version)| (name.to_string(), super::v050_note_truth::VersionRow { version }))
                .collect(),
        };
        let shipped = table(&[("ui", 10), ("vfx", 7), ("bake", 2)]);
        let live = table(&[("ui", 11), ("vfx", 8), ("bake", 2), ("glyphs", 1)]);
        let good = "## Upgrade\n\n### Format changes on the v0.6 line (built)\n\n| Format | Step |\n|---|---|\n\
                    | `ui` | 10 → 11 |\n| `vfx` | 7 → 8 |\n\nNew formats: `glyphs`.\n";
        assert!(bump_errors(good, &shipped, &live).is_empty());
        let no_ui = good.replace("| `ui` | 10 → 11 |\n", "");
        let wrong = good.replace("7 → 8", "6 → 7");
        let unnamed = good.replace("`glyphs`", "glyphs");
        let first = |text: &str| bump_errors(text, &shipped, &live);
        assert!(first(&no_ui)[0].starts_with("ui:"), "{:?}", first(&no_ui));
        assert!(first(&wrong)[0].starts_with("vfx:"), "{:?}", first(&wrong));
        assert!(first(&unnamed)[0].starts_with("glyphs:"), "{:?}", first(&unnamed));
        // A row in the planned table does not count: the step must be in the built table.
        let planned = no_ui.replace("New formats", "### Planned\n\n| `ui` | 10 → 11 |\n\nNew formats");
        assert_eq!(first(&planned).len(), 1, "{:?}", first(&planned));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "# Fragment: alpha\n\nIntro text.\n\n## New\n\n- **Thing.** First line\n  continued here.\n- Second item.\n\n### Rendering\n\n- Draw call fix.\n\n## Changed\n\n- Changed one.\n\n## Fixed\n\n- Fixed one.\n\n## Upgrade\n\n- `Foo` gains a field `bar`; struct literals need `..Default::default()`.\n\n  A second paragraph of the same item.\n- No API changes.\n\n## Known issues\n\nNone.\n";

    #[test]
    fn parses_sections_items_and_areas() {
        let f = parse_fragment("alpha", FULL);
        assert_eq!(f.title.as_deref(), Some("Fragment: alpha"));
        assert_eq!(f.items(&Kind::New).len(), 3);
        assert_eq!(
            f.items(&Kind::New)[0].text,
            "- **Thing.** First line\n  continued here."
        );
        assert_eq!(f.items(&Kind::New)[2].area, "Rendering");
        assert_eq!(f.items(&Kind::Upgrade).len(), 2);
        assert!(f.items(&Kind::Upgrade)[0].text.contains("A second paragraph"));
        assert_eq!(f.items(&Kind::Known)[0].text, "None.");
    }

    #[test]
    fn code_fences_hide_headings_and_stay_whole() {
        let t = "# t\n\n## Upgrade\n\n- Migrate:\n  ```toml\n  ## not a heading\n\n  - not a bullet\n  ```\n- Next.\n\n## Known issues\n\n- x\n";
        let f = parse_fragment("a", t);
        let up = f.items(&Kind::Upgrade);
        assert_eq!(up.len(), 2);
        assert!(up[0].text.contains("## not a heading") && up[0].text.contains("- not a bullet"));
        assert_eq!(f.items(&Kind::Known).len(), 1);
    }

    #[test]
    fn lint_reports_missing_sections() {
        let f = parse_fragment("beta", "# Beta\n\n## Fixed\n\n- One.\n");
        let rules = lint_fragment("docs/releases/unreleased/beta.md", &f, false);
        assert!(rules.contains(&"missing-heading: ## Upgrade".to_string()));
        assert!(rules.contains(&"missing-heading: ## Known issues".to_string()));
        assert!(!rules.iter().any(|r| r.contains("New, ## Changed")));
        let none = parse_fragment("gamma", "# G\n\n## Upgrade\n\n- x\n\n## Known issues\n\n- y\n");
        assert!(
            lint_fragment("docs/releases/unreleased/gamma.md", &none, false)
                .contains(&"missing-heading: ## New, ## Changed or ## Fixed".to_string())
        );
        let notitle = parse_fragment("d", "## New\n\n- a\n");
        assert!(
            lint_fragment("docs/releases/unreleased/d.md", &notitle, false)
                .contains(&"missing-title".to_string())
        );
    }

    #[test]
    fn empty_upgrade_next_to_a_snapshot_change_is_a_finding() {
        let f = parse_fragment(
            "e",
            "# E\n\n## New\n\n- a\n\n## Upgrade\n\n- No API changes.\n\n## Known issues\n\n- None.\n",
        );
        let rules = lint_fragment("docs/releases/unreleased/e.md", &f, true);
        assert!(rules.iter().any(|r| r.starts_with("empty-upgrade")));
        assert!(
            !lint_fragment("docs/releases/unreleased/e.md", &f, false)
                .iter()
                .any(|r| r.starts_with("empty-upgrade"))
        );
        let real = parse_fragment(
            "e",
            "# E\n\n## New\n\n- a\n\n## Upgrade\n\n- `Foo` gains `bar`.\n\n## Known issues\n\n- None.\n",
        );
        assert!(lint_fragment("docs/releases/unreleased/e.md", &real, true).is_empty());
    }

    #[test]
    fn empty_claims_are_strict() {
        assert!(is_empty_claim("None."));
        assert!(is_empty_claim("- No API changes. Tests only."));
        assert!(is_empty_claim("No host-visible API changes."));
        assert!(!is_empty_claim("None host-visible (`button_order` is private)."));
        assert!(!is_empty_claim("No API removed. Hosts must now call init."));
        assert!(!is_empty_claim("No API removed: `Foo` gains a field."));
        assert!(!is_empty_claim("`Foo` gains a field."));
    }

    #[test]
    fn naming_rule() {
        assert!(naming_problem("docs/releases/unreleased/my-lane.md").is_none());
        assert!(naming_problem("docs/releases/unreleased/v0.2.15-my-lane.md").is_some());
        assert!(naming_problem("docs/releases/unreleased/My_Lane.md").is_some());
        assert!(naming_problem("docs/releases/v0.1.28-my-lane.md").is_none());
        assert!(naming_problem("docs/releases/v0.1.21-open-kit.md").is_some());
        assert!(naming_problem("docs/releases/fragments/v0.2.14/v0.1.28-a.md").is_none());
        assert!(naming_problem("docs/releases/fragments/latest/a.md").is_some());
    }

    /// ADR-7624: a root fragment may carry its release, `v<major>.<minor>.<patch>-<lane>.md`; the legacy
    /// `v0.1.28-` form keeps working, and a bad lane or a malformed version is still a finding.
    #[test]
    fn naming_rule_accepts_a_semver_prefix() {
        assert!(naming_problem("docs/releases/v0.5.1-ai-runtime-fixes.md").is_none());
        assert!(naming_problem("docs/releases/v0.5.1-w9-arc-device-churn.md").is_none());
        assert!(naming_problem("docs/releases/v10.22.333-a.md").is_none());
        assert!(naming_problem("docs/releases/fragments/v0.5.1/v0.5.1-a.md").is_none());
        // Still accepted: the legacy form.
        assert!(naming_problem("docs/releases/v0.1.28-my-lane.md").is_none());
        // Bad: a bad lane name after a good prefix.
        assert!(naming_problem("docs/releases/v0.5.1-My_Lane.md").is_some());
        assert!(naming_problem("docs/releases/v0.5.1-.md").is_some());
        assert!(naming_problem("docs/releases/fragments/v0.5.1/v0.5.1-Bad.md").is_some());
        // Bad: a stale version prefix (before the first versioned release, ADR-7684), the legacy one apart.
        assert!(naming_problem("docs/releases/v0.5.0-lane.md").is_some());
        assert!(naming_problem("docs/releases/v0.4.9-lane.md").is_some());
        assert!(naming_problem("docs/releases/v0.1.27-lane.md").is_some());
        // Bad: not a version prefix.
        assert!(naming_problem("docs/releases/v0.5-lane.md").is_some());
        assert!(naming_problem("docs/releases/v0.5.x-lane.md").is_some());
        assert!(naming_problem("docs/releases/v0.5.1lane.md").is_some());
        assert!(naming_problem("docs/releases/vx.5.1-lane.md").is_some());
        assert!(naming_problem("docs/releases/lane.md").is_some());
        // An unreleased fragment still carries no version.
        assert!(naming_problem("docs/releases/unreleased/v0.5.1-lane.md").is_some());
    }

    #[test]
    fn range_selection() {
        let diff = "A\tdocs/releases/v0.1.28-a.md\nA\tdocs/releases/unreleased/b.md\nM\tdocs/releases/v0.1.28-c.md\n\
                    A\tdocs/releases/v0.2.14-draft.md\nA\tdocs/releases/v0.2.13.md\nA\tdocs/releases/README.md\n\
                    A\tdocs/releases/fragments/v0.2.14/v0.1.28-m.md\nD\tdocs/releases/v0.1.28-m.md\n\
                    A\tdocs/other.md\nA\tdocs/releases/v0.1.28-a.md\n";
        assert_eq!(
            select_fragments(diff, false),
            vec!["docs/releases/unreleased/b.md", "docs/releases/v0.1.28-a.md"]
        );
        let with_modified = select_fragments(diff, true);
        assert!(with_modified.contains(&"docs/releases/v0.1.28-c.md".to_string()));
        assert!(!is_fragment_path("docs/releases/v0.2.13-draft.md"));
        assert!(!is_fragment_path("docs/releases/LAUNCH-template.md"));
    }

    fn two_fragments() -> Vec<Fragment> {
        vec![
            parse_fragment(
                "b-lane",
                "# B\n\n## New\n\n- Shared item.\n- B only.\n\n## Upgrade\n\n- `Foo` gains `bar`.\n\n## Known issues\n\n- None.\n",
            ),
            parse_fragment(
                "a-lane",
                "# A\n\n## New\n\n### Rendering\n\n- Shared item.\n\n## New\n\n- Shared item.\n\n## Upgrade\n\n- `Foo` gains `bar`.\n- No API changes.\n\n## Known issues\n\n- Slow on Adreno: see ADR-1.\n",
            ),
        ]
    }

    #[test]
    fn duplicates_merge_and_counts_match() {
        let info = NoteInfo {
            from: "v0.2.13".into(),
            to: "HEAD".into(),
            tag: Some("v0.2.14".into()),
        };
        let note = build_note(&two_fragments(), &info);
        assert!(note.errors.is_empty(), "{:?}", note.errors);
        assert_eq!(note.expected_items, note.written_items);
        assert_eq!(note.expected_items, 9);
        // The identical General item is merged across both fragments; the Rendering one is its own area.
        assert!(note.text.contains("- Shared item. <!-- src: a-lane, b-lane -->"));
        assert!(
            note.text
                .contains("- `Foo` gains `bar`. <!-- src: a-lane, b-lane -->")
        );
        assert!(note.text.contains("<!-- none: a-lane -->") || note.text.contains("<!-- none: b-lane -->"));
        assert!(note.text.starts_with("# Regia Engine v0.2.14 (draft)"));
        assert!(note.text.contains("## Upgrade: host changes since v0.2.13"));
    }

    #[test]
    fn upgrade_and_known_items_survive_verbatim() {
        let item = "- `Foo` gains a field `bar`; struct literals need `..Default::default()`.\n\n  A second paragraph of the same item.";
        let f = parse_fragment("alpha", FULL);
        let note = build_note(&[f], &NoteInfo::default());
        assert!(note.errors.is_empty(), "{:?}", note.errors);
        assert!(note.text.contains(item));
    }

    #[test]
    fn output_is_deterministic_and_order_independent() {
        let info = NoteInfo::default();
        let mut frags = two_fragments();
        let a = build_note(&frags, &info).text;
        frags.reverse();
        assert_eq!(a, build_note(&frags, &info).text);
        assert!(!a.contains("20"), "no dates or years in the output: {a}");
    }

    #[test]
    fn a_dropped_item_is_a_count_mismatch() {
        let note = build_note(&two_fragments(), &NoteInfo::default());
        let tampered = note.text.replace("<!-- src: b-lane -->", "");
        let names: BTreeSet<String> = ["a-lane", "b-lane"].iter().map(|s| s.to_string()).collect();
        assert!(count_attributions(&tampered, &names) < note.expected_items);
        let quoted = format!("{}\n<!-- src: ... -->\n", note.text);
        assert_eq!(count_attributions(&quoted, &names), note.expected_items);
    }

    #[test]
    fn baseline_splits_old_and_new() {
        let base = parse_baseline("# c\nold.md\tmissing-title\n");
        let found = vec![
            ("old.md".to_string(), "missing-title".to_string()),
            ("new.md".to_string(), "missing-title".to_string()),
        ];
        let (old, new) = split_findings(&found, &base);
        assert_eq!(old.len(), 1);
        assert_eq!(new, vec![("new.md".to_string(), "missing-title".to_string())]);
    }
}

/// Truth checks for the v0.7 upgrade guide (`docs/guide/upgrade-v07.md`, format batch 2, ADR-10183) against the
/// live format registry: the live check the v0.6.0 note held until the v0.6.0 tag. Every format whose version
/// moved since v0.6.0 has a guide table row whose first cell starts with the format's name and which states the
/// step (`13 → 14`), and every format v0.6.0 did not have is named in the guide.
#[cfg(test)]
mod v070_guide_truth {
    use std::fs;
    use std::path::Path;

    use super::v050_note_truth::{VersionTable, version_table};

    /// The upgrade guide of format batch 2.
    const GUIDE: &str = "docs/guide/upgrade-v07.md";

    fn guide() -> String {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        fs::read_to_string(root.join(GUIDE)).expect("read the v0.7 upgrade guide")
    }

    /// Every way `guide` misstates the registry's move from `shipped` (v0.6.0) to `live`.
    fn guide_errors(guide: &str, shipped: &VersionTable, live: &VersionTable) -> Vec<String> {
        let mut errors = Vec::new();
        for (name, now) in &live.formats {
            match shipped.formats.get(name) {
                Some(then) if then.version != now.version => {
                    let cell = format!("| `{name}`");
                    let step = format!("{} → {}", then.version, now.version);
                    if !guide.lines().any(|l| l.starts_with(&cell) && l.contains(&step)) {
                        errors.push(format!(
                            "{name}: v{} in v0.6.0, v{} now, but {GUIDE} has no `{name}` table row stating {step}",
                            then.version, now.version
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    if !guide.contains(&format!("`{name}`")) {
                        errors.push(format!(
                            "{name}: new since v0.6.0 (v{}), but {GUIDE} does not name `{name}`",
                            now.version
                        ));
                    }
                }
            }
        }
        errors
    }

    /// The guide states every format step and names every new format of the v0.7 line.
    #[test]
    fn the_v07_guide_lists_every_format_change_since_v060() {
        let errors = guide_errors(
            &guide(),
            &version_table("docs/formats/format-hashes-v0.6.0.ron"),
            &version_table("docs/formats/format-hashes.ron"),
        );
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// The check fires: a missing row, a wrong step and an unnamed new format are each reported, and a row for a
    /// longer name does not count for a shorter one.
    #[test]
    fn missing_or_wrong_guide_rows_are_reported() {
        let table = |rows: &[(&str, u32)]| VersionTable {
            formats: rows
                .iter()
                .map(|&(name, version)| (name.to_string(), super::v050_note_truth::VersionRow { version }))
                .collect(),
        };
        let shipped = table(&[("scene", 13), ("decal", 2), ("bake", 3)]);
        let live = table(&[("scene", 14), ("decal", 3), ("bake", 3), ("gait", 1)]);
        let good = "| Format | Step |\n|---|---|\n| `scene` (`.level.ron`) | 13 → 14 | text |\n\
                    | `decal` | 2 → 3 |\n\nNew: `gait`.\n";
        assert!(guide_errors(good, &shipped, &live).is_empty());
        let first = |text: &str| guide_errors(text, &shipped, &live);
        let no_scene = good.replace("| `scene` (`.level.ron`) | 13 → 14 | text |\n", "");
        assert!(
            first(&no_scene)[0].starts_with("scene:"),
            "{:?}",
            first(&no_scene)
        );
        let wrong = good.replace("2 → 3", "1 → 2");
        assert!(first(&wrong)[0].starts_with("decal:"), "{:?}", first(&wrong));
        let unnamed = good.replace("`gait`", "gait");
        assert!(first(&unnamed)[0].starts_with("gait:"), "{:?}", first(&unnamed));
        let longer = good.replace("| `decal` |", "| `decal-ron` |");
        assert!(first(&longer)[0].starts_with("decal:"), "{:?}", first(&longer));
    }
}
