// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask adr-index [--check]`: generate `docs/decisions-index.md` from `docs/decisions.md`.
//!
//! The index is one table row per ADR (number, date, status word, relations, the first release
//! note that names it, title) plus two lists the file itself cannot show: the numbers nobody uses
//! and the consistency checks below. It never edits `docs/decisions.md`.
//!
//! Checks (reported in the index, and printed by the command; none fails the command):
//! - a number used twice, or a header with no `Date: … Status: …` line;
//! - a status that names an ADR that does not exist;
//! - `amends` / `amended by` and `supersedes` / `superseded by` stated on one side only;
//! - `Status: superseded` with no `superseded by ADR-N`;
//! - two ADRs with the same title;
//! - an `ADR-N:` heading at a level other than 2 (invisible to the index and to duplicate detection).
//!
//! `--check` fails when the committed index differs from what the command would write. It is not
//! part of `preflight`: lanes append ADRs in parallel, so the lead regenerates the index at the gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

/// The decision log, relative to the workspace root.
pub const DECISIONS: &str = "docs/decisions.md";
/// The generated index, relative to the workspace root.
pub const INDEX: &str = "docs/decisions-index.md";
/// Directory of release notes, relative to the workspace root.
pub const RELEASES: &str = "docs/releases";

/// Arguments of `cargo xtask adr-index`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Write nothing; fail when `docs/decisions-index.md` is out of date.
    #[arg(long)]
    pub check: bool,
}

/// One ADR as the log's header and `Date:` line state it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adr {
    /// The number after `ADR-`.
    pub number: u32,
    /// The header text after the colon.
    pub title: String,
    /// The `Date:` value, or empty when the header has no `Date: … Status: …` line.
    pub date: String,
    /// Everything after `Status:`, or empty when there is none.
    pub status: String,
}

/// How a status line relates one ADR to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relation {
    /// `amends ADR-N`, `addendum to ADR-N`.
    Amends,
    /// `amended by ADR-N`.
    AmendedBy,
    /// `supersedes ADR-N`, `replaces ADR-N`.
    Supersedes,
    /// `superseded by ADR-N`.
    SupersededBy,
}

const KEYWORDS: [(&str, Relation); 6] = [
    ("amended by", Relation::AmendedBy),
    ("amends", Relation::Amends),
    ("addendum to", Relation::Amends),
    ("superseded by", Relation::SupersededBy),
    ("supersedes", Relation::Supersedes),
    ("replaces", Relation::Supersedes),
];

/// Parses every `## ADR-N: title` entry, in file order.
pub fn parse(text: &str) -> Vec<Adr> {
    let mut out: Vec<Adr> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ADR-") {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let title = rest[digits.len()..].strip_prefix(':').map(str::trim);
            if let (Ok(number), Some(title)) = (digits.parse::<u32>(), title) {
                out.push(Adr {
                    number,
                    title: title.to_string(),
                    date: String::new(),
                    status: String::new(),
                });
                open = true;
                continue;
            }
        }
        if open
            && let Some(rest) = line.strip_prefix("Date:")
            && let Some((date, status)) = rest.split_once("Status:")
            && let Some(adr) = out.last_mut()
        {
            adr.date = date.trim().to_string();
            adr.status = status.trim().to_string();
            open = false;
        }
    }
    out
}

/// Heading-shaped lines that `parse` cannot see: `ADR-N:` at a level other than 2 (`# ADR-5550: …`,
/// `### ADR-700: …`), one message per line with its 1-based line number. Such an entry is missing from
/// the index and from duplicate detection. A level-2 `## ADR-N:` heading is an entry and is not
/// reported; a sub-heading without a colon after the number (`### ADR-4854 integration addendum`) is
/// not an entry either and is not reported.
pub fn heading_level_problems(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let level = line.bytes().take_while(|&b| b == b'#').count();
        if level == 0 || level == 2 || level > 6 {
            continue;
        }
        let Some(rest) = line[level..].strip_prefix(" ADR-") else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || !rest[digits.len()..].starts_with(':') {
            continue;
        }
        out.push(format!(
            "line {}: `ADR-{digits}` is a level-{level} heading; ADR entries must be `## ADR-N: …`",
            index + 1
        ));
    }
    out
}

/// The first word of a status, lowercased (`accepted`, `proposed`, `superseded`), or `unknown`.
pub fn state(status: &str) -> String {
    let word: String = status.chars().take_while(char::is_ascii_alphabetic).collect();
    if word.is_empty() {
        "unknown".to_string()
    } else {
        word.to_ascii_lowercase()
    }
}

/// The relations a status line states, in text order. `amends ADR-195 and ADR-210` yields both.
pub fn relations(status: &str) -> Vec<(Relation, u32)> {
    let lower = status.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut last: Option<Relation> = None;
    let mut from = 0;
    while let Some(p) = lower[from..].find("adr-") {
        let start = from + p;
        let digits: String = lower[start + 4..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        from = start + 4 + digits.len();
        let Ok(number) = digits.parse::<u32>() else {
            continue;
        };
        let before = lower[..start].trim_end();
        let direct = KEYWORDS
            .iter()
            .find(|(k, _)| before.ends_with(k))
            .map(|(_, r)| *r);
        let kind = direct.or(if before.ends_with(" and") || before.ends_with(',') {
            last
        } else {
            None
        });
        if let Some(kind) = kind {
            out.push((kind, number));
        }
        last = kind;
    }
    out
}

/// Every ADR number a text names as `ADR-N`.
pub fn mentions(text: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    let mut from = 0;
    while let Some(p) = text[from..].find("ADR-") {
        let start = from + p + 4;
        let digits: String = text[start..].chars().take_while(char::is_ascii_digit).collect();
        from = start + digits.len();
        if let Ok(n) = digits.parse::<u32>() {
            out.insert(n);
        }
    }
    out
}

/// A release-notes file name (`v0.1.19.md`) as a sortable version, or `None` for fragments such as
/// `v0.1.19-hair-cards.md`.
pub fn release_version(file_name: &str) -> Option<(Vec<u32>, String)> {
    let stem = file_name.strip_suffix(".md")?.strip_prefix('v')?;
    let parts: Option<Vec<u32>> = stem.split('.').map(|p| p.parse::<u32>().ok()).collect();
    parts.map(|p| (p, format!("v{stem}")))
}

/// The first release (lowest version) whose notes name each ADR. `releases` is `(file name, text)`.
pub fn first_release(releases: &[(String, String)]) -> BTreeMap<u32, String> {
    let mut versions: Vec<(Vec<u32>, String, BTreeSet<u32>)> = releases
        .iter()
        .filter_map(|(name, text)| release_version(name).map(|(key, label)| (key, label, mentions(text))))
        .collect();
    versions.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = BTreeMap::new();
    for (_, label, named) in versions {
        for n in named {
            out.entry(n).or_insert_with(|| label.clone());
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EdgeKind {
    Amend,
    Supersede,
}

/// The consistency problems of the log, one line each. Empty when the log is clean.
pub fn diagnostics(adrs: &[Adr]) -> Vec<String> {
    let mut out = Vec::new();
    let mut by_number: BTreeMap<u32, Vec<&Adr>> = BTreeMap::new();
    for adr in adrs {
        by_number.entry(adr.number).or_default().push(adr);
    }
    for (n, group) in &by_number {
        if group.len() > 1 {
            let titles: Vec<&str> = group.iter().map(|a| a.title.as_str()).collect();
            out.push(format!(
                "ADR-{n:03} is numbered {} times: {}",
                group.len(),
                titles.join(" / ")
            ));
        }
    }
    for adr in adrs {
        if adr.date.is_empty() {
            out.push(format!("ADR-{:03} has no `Date: … Status: …` line", adr.number));
        }
    }
    // (kind, older, newer) -> (stated by the newer ADR, stated by the older ADR)
    let mut edges: BTreeMap<(EdgeKind, u32, u32), (bool, bool)> = BTreeMap::new();
    for adr in adrs {
        let rels = relations(&adr.status);
        for (rel, other) in &rels {
            if !by_number.contains_key(other) {
                out.push(format!(
                    "ADR-{:03} names ADR-{other:03}, which does not exist",
                    adr.number
                ));
                continue;
            }
            let (kind, key, from_newer) = match rel {
                Relation::Amends => (EdgeKind::Amend, (*other, adr.number), true),
                Relation::AmendedBy => (EdgeKind::Amend, (adr.number, *other), false),
                Relation::Supersedes => (EdgeKind::Supersede, (*other, adr.number), true),
                Relation::SupersededBy => (EdgeKind::Supersede, (adr.number, *other), false),
            };
            let entry = edges.entry((kind, key.0, key.1)).or_insert((false, false));
            if from_newer {
                entry.0 = true;
            } else {
                entry.1 = true;
            }
        }
        if state(&adr.status) == "superseded" && !rels.iter().any(|(r, _)| *r == Relation::SupersededBy) {
            out.push(format!(
                "ADR-{:03} is superseded but names no `superseded by ADR-N`",
                adr.number
            ));
        }
    }
    for ((kind, older, newer), (by_newer, by_older)) in &edges {
        let (verb, passive) = match kind {
            EdgeKind::Amend => ("amends", "amended by"),
            EdgeKind::Supersede => ("supersedes", "superseded by"),
        };
        if !by_newer {
            out.push(format!(
                "ADR-{older:03} says {passive} ADR-{newer:03}, which does not say it {verb} ADR-{older:03}"
            ));
        }
        if !by_older {
            out.push(format!(
                "ADR-{newer:03} says {verb} ADR-{older:03}, which does not say {passive} ADR-{newer:03}"
            ));
        }
    }
    let mut titles: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for adr in adrs {
        titles
            .entry(adr.title.trim().to_ascii_lowercase())
            .or_default()
            .push(adr.number);
    }
    for numbers in titles.values() {
        if numbers.len() > 1 {
            let list: Vec<String> = numbers.iter().map(|n| format!("ADR-{n:03}")).collect();
            out.push(format!("same title: {}", list.join(", ")));
        }
    }
    out
}

fn ranges(numbers: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < numbers.len() {
        let mut j = i;
        while j + 1 < numbers.len() && numbers[j + 1] == numbers[j] + 1 {
            j += 1;
        }
        parts.push(if j == i {
            numbers[i].to_string()
        } else {
            format!("{}..{}", numbers[i], numbers[j])
        });
        i = j + 1;
    }
    parts.join(", ")
}

fn relation_cell(status: &str) -> String {
    let words = |r: Relation| match r {
        Relation::Amends => "amends",
        Relation::AmendedBy => "amended by",
        Relation::Supersedes => "supersedes",
        Relation::SupersededBy => "superseded by",
    };
    let parts: Vec<String> = relations(status)
        .iter()
        .map(|(r, n)| format!("{} {n:03}", words(*r)))
        .collect();
    parts.join("; ")
}

/// Renders the index for a decision log (`text` is the raw log, scanned for entries the parser cannot
/// see) and the release notes (`(file name, text)`).
pub fn render(adrs: &[Adr], releases: &[(String, String)], text: &str) -> String {
    let first = first_release(releases);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for adr in adrs {
        *counts.entry(state(&adr.status)).or_default() += 1;
    }
    let mut lines: Vec<String> =
        vec![
        "# ADR index".into(),
        String::new(),
        "<!-- Generated by `cargo xtask adr-index` from docs/decisions.md. Do not edit. -->".into(),
        String::new(),
        format!(
            "{} ADRs in [`decisions.md`](decisions.md). Status words: {}.",
            adrs.len(),
            counts.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")
        ),
        String::new(),
        "The status word is the first word of the ADR's `Status:` field. `Relations` come from that field \
         (`amends`, `amended by`, `supersedes`, `superseded by`). `First named in` is the oldest release \
         note under `docs/releases/` that mentions the ADR; it is a pointer, not proof that the ADR \
         shipped. `proposed` on an older ADR usually means nobody promoted it, not that it is open."
            .into(),
        String::new(),
        "## Index".into(),
        String::new(),
        "| ADR | Date | Status | Relations | First named in | Title |".into(),
        "|---|---|---|---|---|---|".into(),
    ];
    let mut sorted: Vec<&Adr> = adrs.iter().collect();
    sorted.sort_by_key(|a| a.number);
    for adr in sorted {
        lines.push(format!(
            "| {:03} | {} | {} | {} | {} | {} |",
            adr.number,
            adr.date,
            state(&adr.status),
            relation_cell(&adr.status),
            first.get(&adr.number).map_or("-", String::as_str),
            adr.title.replace('|', "\\|"),
        ));
    }
    let used: BTreeSet<u32> = adrs.iter().map(|a| a.number).collect();
    let top = used.last().copied().unwrap_or(0);
    let unused: Vec<u32> = (1..=top).filter(|n| !used.contains(n)).collect();
    lines.extend([String::new(), "## Unused numbers".into(), String::new()]);
    lines.push(if unused.is_empty() {
        "None below the highest number.".into()
    } else {
        format!(
            "Below ADR-{top:03}: {}. Gaps are numbers no merged ADR uses.",
            ranges(&unused)
        )
    });
    let mut problems = diagnostics(adrs);
    problems.extend(heading_level_problems(text));
    lines.extend([String::new(), "## Checks".into(), String::new()]);
    if problems.is_empty() {
        lines.push("No problems found.".into());
    } else {
        lines.extend(problems.iter().map(|p| format!("- {p}")));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Runs the command against the workspace at `root`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let decisions =
        std::fs::read_to_string(root.join(DECISIONS)).with_context(|| format!("reading {DECISIONS}"))?;
    let mut releases: Vec<(String, String)> = Vec::new();
    let dir = root.join(RELEASES);
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry
            .with_context(|| format!("listing {}", dir.display()))?
            .path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if release_version(&name).is_some() {
            let text =
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            releases.push((name, text));
        }
    }
    // Fragments archived by `release-notes --archive` count as the notes of their tag (ADR-2767).
    if let Ok(tags) = std::fs::read_dir(dir.join("fragments")) {
        let mut tag_dirs: Vec<_> = tags.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        tag_dirs.sort();
        for tag_dir in tag_dirs {
            let Some(tag) = tag_dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            let label = format!("{tag}.md");
            if release_version(&label).is_none() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&tag_dir) else {
                continue;
            };
            let mut paths: Vec<_> = files.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            paths.sort();
            for path in paths
                .into_iter()
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
            {
                let text =
                    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                releases.push((label.clone(), text));
            }
        }
    }
    let adrs = parse(&decisions);
    let text = render(&adrs, &releases, &decisions);
    let mut problems = diagnostics(&adrs);
    problems.extend(heading_level_problems(&decisions));
    for problem in &problems {
        println!("adr-index: {problem}");
    }
    let path = root.join(INDEX);
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current == text {
        println!("adr-index: {INDEX} is up to date ({} ADRs)", adrs.len());
        return Ok(ExitCode::SUCCESS);
    }
    if args.check {
        eprintln!("{INDEX} is out of date; run `cargo xtask adr-index`");
        return Ok(ExitCode::FAILURE);
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    println!("adr-index: wrote {INDEX} ({} ADRs)", adrs.len());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
# Decisions

## ADR-001: First thing
Date: 2026-10-03   Status: accepted, amended by ADR-002 (detail)
Context: x

## ADR-002: Second | thing
Date: 2026-10-04   Status: accepted (lane x; amends ADR-001 and ADR-003)
Decision: y

## ADR-003: Third
Date: 2026-10-05   Status: superseded by ADR-004

## ADR-004: Fourth
Date: 2026-10-06   Status: proposed
";

    #[test]
    fn parse_reads_headers_and_status_lines() {
        let adrs = parse(LOG);
        let numbers: Vec<u32> = adrs.iter().map(|a| a.number).collect();
        assert_eq!(numbers, [1, 2, 3, 4]);
        assert_eq!(adrs[1].title, "Second | thing");
        assert_eq!(adrs[0].date, "2026-10-03");
        assert_eq!(state(&adrs[2].status), "superseded");
    }

    #[test]
    fn relations_carry_over_an_and() {
        let rels = relations("accepted (lane x; amends ADR-195 and ADR-210)");
        assert_eq!(rels, [(Relation::Amends, 195), (Relation::Amends, 210)]);
        assert_eq!(
            relations("superseded by ADR-272 (builder)"),
            [(Relation::SupersededBy, 272)]
        );
        assert!(relations("accepted, raised to 30 s by ADR-276").is_empty());
    }

    #[test]
    fn diagnostics_find_one_sided_and_missing_links() {
        let problems = diagnostics(&parse(LOG));
        // 003 is superseded by 004, and 004 does not say it supersedes 003.
        assert!(
            problems
                .iter()
                .any(|p| p.contains("ADR-003 says superseded by ADR-004")),
            "{problems:?}"
        );
        // 002 amends 003, and 003 does not say it is amended by 002.
        assert!(
            problems.iter().any(|p| p.contains("ADR-002 says amends ADR-003")),
            "{problems:?}"
        );
        // 001 <-> 002 is stated on both sides.
        assert!(!problems.iter().any(|p| p.contains("ADR-001")), "{problems:?}");
    }

    #[test]
    fn diagnostics_find_duplicates_dangling_and_unlinked_superseded() {
        let log = "## ADR-001: A\nDate: d   Status: accepted\n\n## ADR-001: B\nDate: d   Status: superseded\n\n\
                   ## ADR-002: a\nDate: d   Status: accepted; amends ADR-009\n\n## ADR-003: No status\n";
        let problems = diagnostics(&parse(log));
        assert!(
            problems.iter().any(|p| p.contains("ADR-001 is numbered 2 times")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("names no `superseded by")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("ADR-009, which does not exist")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("ADR-003 has no")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("same title: ADR-001, ADR-002")),
            "{problems:?}"
        );
    }

    #[test]
    fn first_release_picks_the_lowest_version_and_skips_fragments() {
        let releases = vec![
            ("v0.1.10.md".to_string(), "see ADR-5 and ADR-007".to_string()),
            ("v0.1.9.md".to_string(), "ADR-007 here".to_string()),
            ("v0.1.9-extra.md".to_string(), "ADR-5".to_string()),
        ];
        let first = first_release(&releases);
        assert_eq!(first.get(&7).map(String::as_str), Some("v0.1.9"));
        assert_eq!(first.get(&5).map(String::as_str), Some("v0.1.10"));
    }

    #[test]
    fn render_lists_every_adr_gaps_and_is_stable() {
        let adrs = parse(LOG);
        let text = render(&adrs, &[("v0.1.1.md".to_string(), "ADR-004".to_string())], LOG);
        assert!(
            text.contains("| 004 | 2026-10-06 | proposed |  | v0.1.1 | Fourth |"),
            "{text}"
        );
        assert!(
            text.contains("| 002 | 2026-10-04 | accepted | amends 001; amends 003 | - | Second \\| thing |"),
            "{text}"
        );
        assert!(text.contains("4 ADRs"), "{text}");
        assert!(text.contains("None below the highest number."), "{text}");
        assert_eq!(
            text,
            render(&adrs, &[("v0.1.1.md".to_string(), "ADR-004".to_string())], LOG)
        );
        assert_eq!(ranges(&[3, 4, 5, 9, 11, 12]), "3..5, 9, 11..12");
    }

    #[test]
    fn heading_level_problems_flag_wrong_levels_and_skip_entries_and_subheadings() {
        let text = "\
# ADR-5550: level one
Date: d   Status: proposed

## ADR-002: a proper entry
Date: d   Status: accepted

### ADR-4854 integration addendum
#### ADR-700: four hashes
Date: d   Status: accepted

###### ADR-999999: six hashes

####### ADR-123: seven hashes is not a heading level

# ADR-: no digits

### ADR-71x: not a plain number
";
        let problems = heading_level_problems(text);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`ADR-5550`") && p.contains("level-1")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`ADR-700`") && p.contains("level-4")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`ADR-999999`") && p.contains("level-6")),
            "{problems:?}"
        );
        assert!(problems.iter().any(|p| p.contains("line 1:")), "{problems:?}");
        // The index's Checks section reports the same problems.
        let rendered = render(&parse(text), &[], text);
        assert!(rendered.contains("level-1 heading"), "{rendered}");
        assert!(
            rendered.contains("ADR entries must be `## ADR-N: …`"),
            "{rendered}"
        );
    }

    #[test]
    fn heading_level_problems_are_absent_from_a_clean_log() {
        assert!(heading_level_problems(LOG).is_empty());
        let rendered = render(&parse(LOG), &[], LOG);
        assert!(!rendered.contains("heading; ADR entries must"), "{rendered}");
    }

    /// v061-editor-fonts-notices appended a second `## ADR-8862:` (line ~76366). Number 8862 is
    /// already the v0.6.0 `stream_not_cooked` ADR (line ~50303). `cargo xtask adr-index` prints
    /// the duplicate and does not fail on it; this test does.
    #[test]
    fn adr_8862_is_used_once() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let text = std::fs::read_to_string(root.join(DECISIONS)).expect("decisions.md");
        let parsed = parse(&text);
        let titles: Vec<&str> = parsed
            .iter()
            .filter(|adr| adr.number == 8862)
            .map(|adr| adr.title.as_str())
            .collect();
        assert_eq!(
            titles.len(),
            1,
            "ADR-8862 is numbered {} times: {}",
            titles.len(),
            titles.join(" / ")
        );
    }
}
