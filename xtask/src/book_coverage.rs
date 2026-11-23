// SPDX-License-Identifier: MPL-2.0
//! `book-coverage`: the shipped surface that `agent-coverage` does not reach has a guide page
//! (`docs/specs/96-guides-book.md`, ADR-1704).
//!
//! Five checks, four comparing the code with a table of `book/COVERAGE.md` and one with the editor guide:
//!
//! 1. **Host extension methods** (§ 5, `| Extension method | Registered by | Agent page |`): every
//!    `MethodSchema { name: "area.name", .. }` literal in non-test code under `crates/*/src` that is
//!    not a runtime schema method has a row naming its crate, a page that names it as a whole token,
//!    and a test-code string literal that calls it. A row for a method nothing registers is stale.
//! 2. **Cargo features** (§ 1, `| Crate | Feature | What it does | Page |`): every `[features]` key of
//!    every `crates/*/Cargo.toml` is a backticked token of the Feature cell of a row for its crate,
//!    and that row links a page that exists.
//! 3. **CLI commands** (§ 3, `| Command | What it does | Page |`): every variant of `enum CliCommand`
//!    in `crates/regia_cli/src/main.rs` has a row whose command cell starts with `` `regia <name>``.
//! 4. **Content formats** (§ 4, `| Format | Extension | What it stores | Page |`): every frozen format
//!    of `docs/formats/format-hashes.ron` is named (as itself, or with `-` as `_` or dropped) in the
//!    Format or Extension cell of a row linking a page that exists.
//! 5. **Editor panels** (`book/user-guide/editor-guide/index.md`, `| Panel variant | Tab | Page |` and
//!    `| Panel variant | Why it has no page of its own |`): every variant of `enum Panel` in
//!    `crates/regia_editor/src/shell.rs` has a row linking a page that exists, or a reasoned exemption row.
//!
//! Test code follows the `agent-coverage` rules: files under `tests/`, `#![cfg(test)]` files,
//! `#[cfg(test)]` items and the files those pull in with `mod name;`. The runtime tests of the
//! registering crates (`regia_editor`, `regia_steam`, `regia_agent`, `regia_cli`) check the same
//! tables against the methods and commands they really register, which pins this text scan.
//!
//! `book-check` runs these checks when `crates/regia_agent/src/dispatch.rs` exists (the real tree;
//! book fixtures skip them). `cargo xtask book-coverage [--json]` runs them on their own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::agent_coverage::{
    external_mod_name, external_mods, is_keyword, is_whole_test_file, match_end, normalize_page, read_ident,
    resolve_mod, skip_char_or_lifetime, skip_comment, skip_ws_and_comments, take_string, test_literals,
    test_spans, utf8_len, walk_rs,
};

/// The version of the `--json` report shape (spec 96 § 4).
pub const REPORT_FORMAT_VERSION: u32 = 1;

/// The header cells of the extension-method table (§ 5 of `book/COVERAGE.md`).
pub const EXTENSION_HEADER: [&str; 3] = ["Extension method", "Registered by", "Agent page"];

/// The header cells of the crate-feature table (§ 1).
pub const FEATURE_HEADER: [&str; 4] = ["Crate", "Feature", "What it does", "Page"];

/// The header cells of the CLI table (§ 3).
pub const CLI_HEADER: [&str; 3] = ["Command", "What it does", "Page"];

/// The header cells of the content-format table (§ 4).
pub const FORMAT_HEADER: [&str; 4] = ["Format", "Extension", "What it stores", "Page"];

/// The header cells of the editor guide's panel table: one row per `Panel` variant that has a page.
pub const PANEL_HEADER: [&str; 3] = ["Panel variant", "Tab", "Page"];

/// The header cells of the editor guide's table of panels with no page of their own (internal or
/// debug panels, and panels whose page is still to be written), each with its reason.
pub const PANEL_EXEMPT_HEADER: [&str; 2] = ["Panel variant", "Why it has no page of its own"];

/// The file that declares `enum Panel`, relative to the root.
pub const PANEL_SOURCE: &str = "crates/regia_editor/src/shell.rs";

/// The page that holds both panel tables, relative to the root.
pub const PANEL_GUIDE: &str = "book/user-guide/editor-guide/index.md";

/// The folder of [`PANEL_GUIDE`] relative to `book/`: the base of the links in its tables.
const PANEL_GUIDE_DIR: &str = "user-guide/editor-guide";

/// The committed frozen-format registry (`cargo xtask check-formats`), relative to the root.
pub const FORMAT_REGISTRY: &str = "docs/formats/format-hashes.ron";

/// Which table a finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// Host extension methods (§ 5).
    Extension,
    /// Cargo features (§ 1).
    Feature,
    /// `regia` CLI subcommands (§ 3).
    Cli,
    /// Frozen content formats (§ 4).
    Format,
    /// Editor `Panel` variants against the editor guide's panel table.
    Panel,
    /// Ignored Rust fences against the committed baseline.
    Fence,
}

impl Section {
    /// The name the JSON report uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Feature => "feature",
            Self::Cli => "cli",
            Self::Format => "format",
            Self::Panel => "panel",
            Self::Fence => "fence",
        }
    }
}

/// One gap or drift between the code and the book.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// The table it is about.
    pub section: Section,
    /// A stable lowercase code (spec 96 § 5): `missing_row`, `missing_page`,
    /// `page_does_not_mention`, `untested`, `wrong_registrar`, `stale_row`, `wrong_section`,
    /// `ignored_fence_grew`, `shipped_tool_unchecked`.
    pub code: &'static str,
    /// What it is about: a method, `crate/feature`, or `regia <command>`.
    pub subject: String,
    /// One line for a human.
    pub message: String,
}

/// What [`check`] found.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Every shipped host extension method and the crate (directory under `crates/`) that
    /// registers it.
    pub extension_methods: BTreeMap<String, String>,
    /// Every workspace crate under `crates/` with features, and its features (sorted).
    pub features: BTreeMap<String, Vec<String>>,
    /// The top-level `regia` subcommands, in declaration order.
    pub cli_commands: Vec<String>,
    /// The nested `regia` subcommand paths (`steam upload`, `procgen graph check`), sorted.
    pub cli_nested: Vec<String>,
    /// Every frozen content format of the committed registry and its current version.
    pub formats: BTreeMap<String, u32>,
    /// Every gap, sorted and deduplicated. Empty when the book covers the surface.
    pub findings: Vec<Finding>,
    /// Observations that do not fail the check (a feature row whose tokens name no Cargo feature).
    pub notes: Vec<String>,
    /// The book's Rust fences: how many `book-check` compiles and how many are `rust,ignore`.
    pub fences: FenceCounts,
}

/// The book's Rust code fences (spec 96 § 11: the ignored count is held to `book/fence-baseline.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FenceCounts {
    /// Bare `rust` fences (and `no_run`, `should_panic`): compiled by `book-check`.
    pub compiled: usize,
    /// `rust,ignore` fences: not compiled.
    pub ignored: usize,
    /// `rust,ignore` fences per page (relative to `book/`), pages with none left out.
    pub ignored_by_page: BTreeMap<String, usize>,
    /// `rust,ignore` fences without an `ignore-reason` comment, per page: what the ratchet
    /// compares with `book/fence-baseline.json`.
    pub unjustified_by_page: BTreeMap<String, usize>,
}

/// The committed ignored-fence baseline, relative to the root (ADR-2648).
pub const FENCE_BASELINE: &str = "book/fence-baseline.json";

/// The version of [`FENCE_BASELINE`].
pub const FENCE_BASELINE_VERSION: u32 = 1;

/// Every fenced block of `text` as `(info string, has an ignore-reason)`, in order. A fence has a
/// reason when the nearest non-blank line before it is an HTML comment holding
/// `ignore-reason: <text>`.
pub(crate) fn fences_with_reason(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut previous = "";
    for line in text.lines() {
        let trimmed = line.trim_start();
        if in_fence {
            if trimmed.starts_with("```") {
                in_fence = false;
                previous = "";
            }
            continue;
        }
        if let Some(info) = trimmed.strip_prefix("```") {
            in_fence = true;
            let reason = previous
                .split_once("ignore-reason:")
                .is_some_and(|(_, r)| !r.trim().trim_end_matches("-->").trim().is_empty());
            out.push((info.trim().to_string(), reason));
        } else if !trimmed.trim().is_empty() {
            previous = trimmed;
        }
    }
    out
}

/// The ratchet: a page may not have more unjustified `rust,ignore` fences than its baseline entry
/// (0 for a page without one). Pages under the baseline are a note, so a lane that converts fences
/// can lower the file; it never has to.
pub(crate) fn fence_findings(
    counts: &FenceCounts,
    baseline: &BTreeMap<String, usize>,
) -> (Vec<Finding>, Vec<String>) {
    let mut findings = Vec::new();
    let mut notes = Vec::new();
    for (page, &count) in &counts.unjustified_by_page {
        let allowed = baseline.get(page).copied().unwrap_or(0);
        if count > allowed {
            findings.push(finding(
                Section::Fence,
                "ignored_fence_grew",
                format!("book/{page}"),
                format!(
                    "book/{page} has {count} `rust,ignore` fences, its baseline in {FENCE_BASELINE} is {allowed}: make the new fence compile (`rust`, or `rust,wrap` with hidden `# ` setup lines) or put `<!-- ignore-reason: why -->` on the line before it"
                ),
            ));
        }
    }
    for (page, &allowed) in baseline {
        let count = counts.unjustified_by_page.get(page).copied().unwrap_or(0);
        if count < allowed {
            notes.push(format!(
                "{FENCE_BASELINE}: book/{page} has {count} ignored fences, the baseline says {allowed}; lower it"
            ));
        }
    }
    (findings, notes)
}

/// Reads [`FENCE_BASELINE`]: `{ "format_version": 1, "ignored_by_page": { "<page>": n } }`. An
/// absent file is an empty baseline.
pub fn read_fence_baseline(root: &Path) -> Result<BTreeMap<String, usize>> {
    let path = root.join(FENCE_BASELINE);
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse_fence_baseline(&text).with_context(|| format!("parsing {FENCE_BASELINE}"))
}

/// Parses the baseline text (see [`read_fence_baseline`]).
pub(crate) fn parse_fence_baseline(text: &str) -> Result<BTreeMap<String, usize>> {
    let value: Value = serde_json::from_str(text)?;
    anyhow::ensure!(
        value["format_version"].as_u64() == Some(u64::from(FENCE_BASELINE_VERSION)),
        "format_version must be {FENCE_BASELINE_VERSION}"
    );
    let pages = value["ignored_by_page"]
        .as_object()
        .context("`ignored_by_page` must be an object")?;
    pages
        .iter()
        .map(|(page, n)| {
            n.as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .map(|n| (page.clone(), n))
                .with_context(|| format!("`{page}`: the count must be a non-negative integer"))
        })
        .collect()
}

/// The baseline file text for `counts` (sorted, one page per line), for a lane that lowers it.
#[must_use]
pub fn fence_baseline_text(counts: &FenceCounts) -> String {
    let mut text =
        format!("{{\n  \"format_version\": {FENCE_BASELINE_VERSION},\n  \"ignored_by_page\": {{\n");
    let last = counts.unjustified_by_page.len();
    for (i, (page, n)) in counts.unjustified_by_page.iter().enumerate() {
        let comma = if i + 1 == last { "" } else { "," };
        text.push_str(&format!("    \"{page}\": {n}{comma}\n"));
    }
    text.push_str("  }\n}\n");
    text
}

/// Counts the Rust fences of every page under `book`.
pub fn fence_counts(book: &Path) -> Result<FenceCounts> {
    let mut counts = FenceCounts::default();
    if !book.is_dir() {
        return Ok(counts);
    }
    for rel in crate::book::book_markdown(book)? {
        let text = std::fs::read_to_string(book.join(&rel))
            .with_context(|| format!("reading book/{}", rel.display()))?;
        for (info, reason) in fences_with_reason(&text) {
            let page = rel.to_string_lossy().replace('\\', "/");
            if crate::book::is_rust(&info) {
                counts.compiled += 1;
            } else if info.split(',').next().map(str::trim) == Some("rust") {
                counts.ignored += 1;
                *counts.ignored_by_page.entry(page.clone()).or_default() += 1;
                if !reason {
                    *counts.unjustified_by_page.entry(page).or_default() += 1;
                }
            }
        }
    }
    Ok(counts)
}

impl Report {
    /// The `--json` report (spec 96 § 5).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let findings: Vec<Value> = self
            .findings
            .iter()
            .map(|f| {
                json!({
                    "section": f.section.name(),
                    "code": f.code,
                    "subject": f.subject,
                    "message": f.message,
                })
            })
            .collect();
        json!({
            "format_version": REPORT_FORMAT_VERSION,
            "ok": self.findings.is_empty(),
            "counts": {
                "extension_methods": self.extension_methods.len(),
                "features": self.features.values().map(Vec::len).sum::<usize>(),
                "cli_commands": self.cli_commands.len(),
                "cli_nested_commands": self.cli_nested.len(),
                "formats": self.formats.len(),
            },
            "extension_methods": self.extension_methods,
            "findings": findings,
            "notes": self.notes,
            "fences": {
                "compiled": self.fences.compiled,
                "ignored": self.fences.ignored,
                "ignored_by_page": self.fences.ignored_by_page,
                "unjustified_by_page": self.fences.unjustified_by_page,
            },
        })
    }
}

/// Runs every check over the workspace rooted at `root`.
pub fn check(root: &Path) -> Result<Report> {
    let coverage_path = root.join("book/COVERAGE.md");
    let coverage = std::fs::read_to_string(&coverage_path)
        .with_context(|| format!("reading {}", coverage_path.display()))?;
    let book = root.join("book");
    let schema: BTreeSet<&str> = regia_agent::schema::METHODS.iter().map(|m| m.name).collect();

    let mut report = Report {
        extension_methods: extension_methods(root, &schema)?,
        features: workspace_features(root)?,
        formats: registry_formats(root)?,
        fences: fence_counts(&book)?,
        ..Report::default()
    };
    let cli_path = root.join("crates/regia_cli/src/main.rs");
    if cli_path.is_file() {
        let source =
            std::fs::read_to_string(&cli_path).with_context(|| format!("reading {}", cli_path.display()))?;
        report.cli_commands = cli_commands(&source).map_err(anyhow::Error::msg)?;
        let mut sources = cli_sources(root)?;
        sources.push(source.clone());
        report.cli_nested = nested_cli_commands(&source, &sources).map_err(anyhow::Error::msg)?;
    }

    let tested = test_literals(root)?;
    let mut pages = PageCache::new(&book);
    let mut findings = extension_findings(
        &report.extension_methods,
        &schema,
        &table_rows(&coverage, &EXTENSION_HEADER),
        &tested,
        &mut pages,
    );
    let (feature_findings, mut notes) = feature_findings(
        &report.features,
        &table_rows(&coverage, &FEATURE_HEADER),
        root,
        &mut pages,
    );
    findings.extend(feature_findings);
    findings.extend(cli_findings(
        &report.cli_commands,
        &table_rows(&coverage, &CLI_HEADER),
        &mut pages,
    ));
    findings.extend(tool_manifest_findings(&shipped_tool_manifests(root)?));
    findings.extend(nested_cli_findings(
        &report.cli_commands,
        &report.cli_nested,
        &table_rows(&coverage, &CLI_HEADER),
    ));
    findings.extend(format_findings(
        &report.formats,
        &table_rows(&coverage, &FORMAT_HEADER),
        &mut pages,
    ));
    let panel_source = root.join(PANEL_SOURCE);
    let panel_guide = root.join(PANEL_GUIDE);
    if panel_source.is_file() && panel_guide.is_file() {
        let source = std::fs::read_to_string(&panel_source)
            .with_context(|| format!("reading {}", panel_source.display()))?;
        let guide = std::fs::read_to_string(&panel_guide)
            .with_context(|| format!("reading {}", panel_guide.display()))?;
        let variants = panel_variants(&source).map_err(anyhow::Error::msg)?;
        findings.extend(panel_findings(
            &variants,
            &table_rows(&guide, &PANEL_HEADER),
            &table_rows(&guide, &PANEL_EXEMPT_HEADER),
            &mut pages,
        ));
    }
    let known_methods: BTreeSet<String> = schema
        .iter()
        .map(|m| (*m).to_string())
        .chain(report.extension_methods.keys().cloned())
        .collect();
    findings.extend(stale_reference_findings(
        root,
        &table_rows(&coverage, &FORMAT_HEADER),
        &known_methods,
    ));
    let (fence_findings, fence_notes) = fence_findings(&report.fences, &read_fence_baseline(root)?);
    findings.extend(fence_findings);
    notes.extend(fence_notes);
    findings.sort();
    findings.dedup();
    report.findings = findings;
    report.notes = notes;
    Ok(report)
}

/// `cargo xtask book-coverage [--json]`: prints the report, exit code 1 on any finding.
pub fn run(root: &Path, as_json: bool) -> Result<ExitCode> {
    let report = check(root)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        for finding in &report.findings {
            eprintln!("error: [{}] {}", finding.code, finding.message);
        }
        for note in &report.notes {
            eprintln!("note: {note}");
        }
        if report.findings.is_empty() {
            println!(
                "book-coverage: ok ({} extension methods, {} features, {} commands, {} formats)",
                report.extension_methods.len(),
                report.features.values().map(Vec::len).sum::<usize>(),
                report.cli_commands.len(),
                report.formats.len()
            );
        } else {
            eprintln!("book-coverage: {} problem(s)", report.findings.len());
        }
    }
    Ok(if report.findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

// ---------------------------------------------------------------------------
// Book pages and tables
// ---------------------------------------------------------------------------

/// Book pages read on demand, by path relative to `book/`.
struct PageCache {
    book: PathBuf,
    pages: BTreeMap<String, Option<String>>,
}

impl PageCache {
    fn new(book: &Path) -> Self {
        Self {
            book: book.to_path_buf(),
            pages: BTreeMap::new(),
        }
    }

    /// The text of `page` (relative to `book/`, `#fragment` dropped), or `None` when it does not
    /// exist or cannot be read.
    fn get(&mut self, page: &str) -> Option<&str> {
        let page = normalize_page(page.split_once('#').map_or(page, |(path, _)| path));
        let book = &self.book;
        self.pages
            .entry(page)
            .or_insert_with_key(|page| std::fs::read_to_string(book.join(page)).ok())
            .as_deref()
    }
}

/// Splits a markdown table row into trimmed cells. An escaped `\|` stays inside its cell.
pub(crate) fn row_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell).trim().to_string()),
            _ => cell.push(c),
        }
    }
    cells.push(cell.trim().to_string());
    cells
}

/// The data rows (as cells) of every table whose header cells are exactly `header`, in order (a
/// section may continue its table after a paragraph by repeating the header). Empty when there is
/// no such table.
pub(crate) fn table_rows(text: &str, header: &[&str]) -> Vec<Vec<String>> {
    let is_header = |line: &str| {
        line.trim_start().starts_with('|')
            && row_cells(line)
                .iter()
                .map(String::as_str)
                .eq(header.iter().copied())
    };
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        if !line.trim_start().starts_with('|') {
            in_table = false;
            continue;
        }
        if is_header(line) {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        let cells = row_cells(line);
        if cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
        {
            continue;
        }
        rows.push(cells);
    }
    rows
}

/// The backticked spans of a cell, in order (`` `a`, `b` `` gives `a`, `b`).
pub(crate) fn backticked(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The `](target)` link targets of a cell.
pub(crate) fn links(cell: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = cell;
    while let Some(at) = rest.find("](") {
        let after = &rest[at + 2..];
        let Some(end) = after.find(')') else { break };
        out.push(after[..end].trim().to_string());
        rest = &after[end..];
    }
    out
}

/// Whether a link points inside the book (not a URL or a same-page anchor).
pub(crate) fn is_book_link(target: &str) -> bool {
    !(target.is_empty() || target.starts_with('#') || target.contains("://") || target.starts_with("mailto:"))
}

// ---------------------------------------------------------------------------
// Host extension methods
// ---------------------------------------------------------------------------

/// Every non-test `.rs` file under `dir`, with its `#[cfg(test)]` items blanked to spaces (byte
/// offsets kept). Files that are test code as a whole (under `tests/`, `#![cfg(test)]`, or pulled
/// in by a `#[cfg(test)] mod name;`) are left out.
pub(crate) fn shipped_sources(root: &Path, dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();
    walk_rs(dir, &mut files)?;
    let mut texts = Vec::with_capacity(files.len());
    let mut test_only: BTreeSet<PathBuf> = BTreeSet::new();
    let mut pending: Vec<PathBuf> = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        if is_whole_test_file(root, &path, &text) {
            pending.extend(external_mods(&path, &text));
            test_only.insert(path);
            continue;
        }
        let spans = test_spans(&text);
        for &(start, end) in &spans {
            if let Some(name) = external_mod_name(&text[start..end]) {
                pending.extend(resolve_mod(&path, name));
            }
        }
        texts.push((path, blank_spans(&text, &spans)));
    }
    while let Some(path) = pending.pop() {
        if !test_only.insert(path.clone()) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            pending.extend(external_mods(&path, &text));
        }
    }
    texts.retain(|(path, _)| !test_only.contains(path));
    Ok(texts)
}

/// `text` with every byte of `spans` replaced by a space (newlines kept). Span ends are char
/// boundaries, so the result is valid UTF-8.
fn blank_spans(text: &str, spans: &[(usize, usize)]) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for &(start, end) in spans {
        out.push_str(&text[at..start]);
        out.extend(text[start..end].chars().flat_map(|c| {
            let fill = if c == '\n' { '\n' } else { ' ' };
            std::iter::repeat_n(fill, if c == '\n' { 1 } else { c.len_utf8() })
        }));
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// Calls `f(at, ident_end)` for every identifier of `text` outside comments, strings and char
/// literals.
fn for_each_ident(text: &str, mut f: impl FnMut(usize, usize)) {
    let mut i = 0;
    while i < text.len() {
        if let Some(end) = skip_comment(text, i) {
            i = end;
            continue;
        }
        if let Some(end) = skip_char_or_lifetime(text, i) {
            i = end;
            continue;
        }
        if let Some((end, _)) = take_string(text, i) {
            i = end;
            continue;
        }
        let starts =
            i == 0 || !(text.as_bytes()[i - 1].is_ascii_alphanumeric() || text.as_bytes()[i - 1] == b'_');
        if starts && let Some((_, end)) = read_ident(text, i) {
            f(i, end);
            i = end;
            continue;
        }
        i += utf8_len(text, i);
    }
}

/// The end of the parenthesised group that opens at `open` (one past its `)`).
fn paren_end(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        if let Some(end) = skip_comment(text, i) {
            i = end;
            continue;
        }
        if let Some((end, _)) = take_string(text, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += utf8_len(text, i);
    }
    None
}

/// The same-file schema constructors: `fn f(name: &'static str, ..) -> MethodSchema`.
fn schema_constructors(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for_each_ident(text, |at, end| {
        if &text[at..end] != "fn" {
            return;
        }
        let name_at = skip_ws_and_comments(text, end);
        let Some((name, after_name)) = read_ident(text, name_at) else {
            return;
        };
        let open = skip_ws_and_comments(text, after_name);
        if !text[open..].starts_with('(') {
            return;
        }
        let Some(close) = paren_end(text, open) else {
            return;
        };
        let first = skip_ws_and_comments(text, open + 1);
        let first_is_name = read_ident(text, first).is_some_and(|(param, next)| {
            param == "name" && text[skip_ws_and_comments(text, next)..].starts_with(':')
        });
        let arrow = skip_ws_and_comments(text, close);
        if !first_is_name || !text[arrow..].starts_with("->") {
            return;
        }
        let ret = skip_ws_and_comments(text, arrow + 2);
        if read_ident(text, ret).is_some_and(|(ty, _)| ty == "MethodSchema") {
            out.insert(name.to_string());
        }
    });
    out
}

/// The string-literal first argument of every call to a constructor of [`schema_constructors`]
/// (`schema("editor.tool_list", ..)`), in order. The definitions are not calls.
fn constructor_call_names(text: &str) -> Vec<String> {
    let constructors = schema_constructors(text);
    let mut names = Vec::new();
    if constructors.is_empty() {
        return names;
    }
    for_each_ident(text, |at, end| {
        if !constructors.contains(&text[at..end]) {
            return;
        }
        let before = text[..at].trim_end();
        if before.ends_with("fn") || before.ends_with('.') {
            return;
        }
        let open = skip_ws_and_comments(text, end);
        if !text[open..].starts_with('(') {
            return;
        }
        let arg = skip_ws_and_comments(text, open + 1);
        if let Some((_, literal)) = take_string(text, arg) {
            names.push(literal);
        }
    });
    names
}

/// Every method name a schema row in `text` declares: the top-level `name: "..."` of each
/// `MethodSchema { .. }` literal, then the first argument of each same-file constructor call
/// ([`constructor_call_names`]).
pub(crate) fn method_schema_names(text: &str) -> Vec<String> {
    let mut names = method_schema_literal_names(text);
    names.extend(constructor_call_names(text));
    names
}

/// The top-level `name: "..."` of every `MethodSchema { .. }` literal in `text`, in order.
/// Comments, strings and char literals are skipped; nested fields (`ParamSchema { name: .. }`)
/// are not the method's name.
fn method_schema_literal_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(end) = skip_comment(text, i) {
            i = end;
            continue;
        }
        if let Some(end) = skip_char_or_lifetime(text, i) {
            i = end;
            continue;
        }
        if let Some((end, _)) = take_string(text, i) {
            i = end;
            continue;
        }
        if is_keyword(text, i, "MethodSchema") {
            let after = skip_ws_and_comments(text, i + "MethodSchema".len());
            if text[after..].starts_with('{') {
                let body_start = after + 1;
                let body_end = match_end(text, body_start).unwrap_or(text.len());
                // `fn f() -> MethodSchema { MethodSchema { .. } }`: a body with no top-level
                // `name` is a function body, not the literal, so scan inside it.
                if let Some(name) = top_level_name(&text[body_start..body_end]) {
                    names.push(name);
                    i = body_end;
                } else {
                    i = body_start;
                }
                continue;
            }
            i += "MethodSchema".len();
            continue;
        }
        i += utf8_len(text, i);
    }
    names
}

/// The string value of the `name:` field at depth 0 of a struct-literal body.
fn top_level_name(body: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < body.len() {
        if let Some(end) = skip_comment(body, i) {
            i = end;
            continue;
        }
        if let Some(end) = skip_char_or_lifetime(body, i) {
            i = end;
            continue;
        }
        if let Some((end, _)) = take_string(body, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth -= 1,
            _ => {}
        }
        if depth == 0
            && is_keyword(body, i, "name")
            && let Some((_, next)) = read_ident(body, i)
        {
            let colon = skip_ws_and_comments(body, next);
            if body[colon..].starts_with(':') && !body[colon..].starts_with("::") {
                let value = skip_ws_and_comments(body, colon + 1);
                if let Some((_, literal)) = take_string(body, value) {
                    return Some(literal);
                }
            }
            i = next;
            continue;
        }
        i += utf8_len(body, i);
    }
    None
}

/// Every host extension method registered by non-test code under `crates/*/src`, mapped to the
/// crate directory that holds its schema literal. Runtime schema methods are left out.
pub fn extension_methods(root: &Path, schema: &BTreeSet<&str>) -> Result<BTreeMap<String, String>> {
    let crates = root.join("crates");
    let mut out = BTreeMap::new();
    if !crates.is_dir() {
        return Ok(out);
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&crates)
        .with_context(|| format!("reading {}", crates.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.join("src").is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let krate = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for (_, text) in shipped_sources(root, &dir.join("src"))? {
            for name in method_schema_names(&text) {
                if !schema.contains(name.as_str()) {
                    out.entry(name).or_insert_with(|| krate.clone());
                }
            }
        }
    }
    Ok(out)
}

fn finding(section: Section, code: &'static str, subject: impl Into<String>, message: String) -> Finding {
    Finding {
        section,
        code,
        subject: subject.into(),
        message,
    }
}

fn extension_findings(
    methods: &BTreeMap<String, String>,
    schema: &BTreeSet<&str>,
    rows: &[Vec<String>],
    tested: &BTreeSet<String>,
    pages: &mut PageCache,
) -> Vec<Finding> {
    let s = Section::Extension;
    let mut out = Vec::new();
    // (method, registered-by cell, agent page cell) per row.
    let parsed: Vec<(String, &str, &str)> = rows
        .iter()
        .map(|cells| {
            let method = backticked(cells.first().map_or("", String::as_str))
                .into_iter()
                .next()
                .unwrap_or_default();
            let by = cells.get(1).map_or("", String::as_str);
            let page = cells.get(2).map_or("", String::as_str);
            (method, by, page)
        })
        .collect();
    for (method, krate) in methods {
        let own: Vec<_> = parsed.iter().filter(|(m, _, _)| m == method).collect();
        if own.is_empty() {
            out.push(finding(
                s,
                "missing_row",
                method,
                format!(
                    "extension method `{method}` (registered in `{krate}`) has no row in the book/COVERAGE.md extension table"
                ),
            ));
        }
        for (_, by, page_cell) in own {
            if !backticked(by).iter().any(|c| c == krate) {
                out.push(finding(
                    s,
                    "wrong_registrar",
                    method,
                    format!("extension method `{method}` is registered in `{krate}`, but its row says {by}"),
                ));
            }
            let targets: Vec<String> = links(page_cell).into_iter().filter(|t| is_book_link(t)).collect();
            if targets.is_empty() {
                out.push(finding(
                    s,
                    "missing_page",
                    method,
                    format!("extension method `{method}`: its row links no agent page"),
                ));
            }
            for target in targets {
                match pages.get(&target) {
                    None => out.push(finding(
                        s,
                        "missing_page",
                        method,
                        format!("extension method `{method}` maps to `{target}`, which does not exist"),
                    )),
                    Some(text) if !regia_agent::mentions_token(text, method) => out.push(finding(
                        s,
                        "page_does_not_mention",
                        method,
                        format!("extension method `{method}` is mapped to `{target}`, but that page does not mention it"),
                    )),
                    Some(_) => {}
                }
            }
        }
        if !tested.contains(method) {
            out.push(finding(
                s,
                "untested",
                method,
                format!(
                    "extension method `{method}` is not called by any test (no string literal in test code)"
                ),
            ));
        }
    }
    for (method, _, _) in &parsed {
        if methods.contains_key(method) {
            continue;
        }
        if schema.contains(method.as_str()) {
            out.push(finding(
                s,
                "wrong_section",
                method,
                format!("`{method}` is a runtime schema method: its row belongs in the method table (§ 2), not the extension table"),
            ));
        } else {
            out.push(finding(
                s,
                "stale_row",
                method,
                format!("the extension table names `{method}`, which no crate registers"),
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Cargo features
// ---------------------------------------------------------------------------

/// The `[features]` keys of every `crates/*/Cargo.toml`, by crate directory name. Crates without
/// features are left out.
pub fn workspace_features(root: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let crates = root.join("crates");
    let mut out = BTreeMap::new();
    if !crates.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&crates).with_context(|| format!("reading {}", crates.display()))? {
        let dir = entry?.path();
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let text =
            std::fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
        let features = manifest_features(&text).with_context(|| format!("parsing {}", manifest.display()))?;
        if !features.is_empty() {
            let krate = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.insert(krate, features);
        }
    }
    Ok(out)
}

/// The sorted `[features]` keys of one manifest.
pub(crate) fn manifest_features(manifest: &str) -> Result<Vec<String>> {
    let value: toml::Value = toml::from_str(manifest)?;
    let mut features: Vec<String> = value
        .get("features")
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default();
    features.sort();
    Ok(features)
}

fn feature_findings(
    features: &BTreeMap<String, Vec<String>>,
    rows: &[Vec<String>],
    root: &Path,
    pages: &mut PageCache,
) -> (Vec<Finding>, Vec<String>) {
    let s = Section::Feature;
    let mut out = Vec::new();
    let mut notes = Vec::new();
    // (crate, feature tokens, page links) per row.
    let parsed: Vec<(String, Vec<String>, Vec<String>)> = rows
        .iter()
        .map(|cells| {
            let krate = backticked(cells.first().map_or("", String::as_str))
                .into_iter()
                .next()
                .unwrap_or_default();
            let tokens = backticked(cells.get(1).map_or("", String::as_str));
            let page = links(cells.last().map_or("", String::as_str))
                .into_iter()
                .filter(|t| is_book_link(t))
                .collect();
            (krate, tokens, page)
        })
        .collect();
    for (krate, list) in features {
        for feature in list {
            let subject = format!("{krate}/{feature}");
            let own: Vec<_> = parsed
                .iter()
                .filter(|(c, tokens, _)| c == krate && tokens.iter().any(|t| t == feature))
                .collect();
            if own.is_empty() {
                out.push(finding(
                    s,
                    "missing_row",
                    &subject,
                    format!(
                        "Cargo feature `{krate}/{feature}` has no row in the book/COVERAGE.md feature table"
                    ),
                ));
            }
            for (_, _, targets) in own {
                if targets.is_empty() {
                    out.push(finding(
                        s,
                        "missing_page",
                        &subject,
                        format!("Cargo feature `{krate}/{feature}`: its row links no page"),
                    ));
                }
                for target in targets {
                    if pages.get(target).is_none() {
                        out.push(finding(
                            s,
                            "missing_page",
                            &subject,
                            format!(
                                "Cargo feature `{krate}/{feature}` maps to `{target}`, which does not exist"
                            ),
                        ));
                    }
                }
            }
        }
    }
    for (krate, tokens, _) in &parsed {
        let Some(list) = features.get(krate) else {
            // A crate with no features (a row about modules) is a note; a crate that is not in
            // `crates/` or `samples/` at all is stale. Samples (`hot_fixture`) are not checked.
            if root.join("crates").join(krate).join("Cargo.toml").is_file() {
                notes.push(format!(
                    "the feature table has a row for `{krate}`, which has no Cargo features"
                ));
            } else if !root.join("samples").join(krate).join("Cargo.toml").is_file()
                && !is_sample_package(root, krate)
            {
                out.push(finding(
                    s,
                    "stale_row",
                    krate,
                    format!(
                        "the feature table has a row for `{krate}`, which is not a crate of this workspace"
                    ),
                ));
            }
            continue;
        };
        if !tokens.iter().any(|t| list.contains(t)) {
            notes.push(format!(
                "the feature table row for `{krate}` ({}) names no Cargo feature of that crate",
                tokens.join(", ")
            ));
        }
    }
    (out, notes)
}

/// Whether some `samples/*/Cargo.toml` (one level deep, or under `samples/*/*`) names package
/// `name`. Sample packages are not always named after their directory (`hot_fixture`).
fn is_sample_package(root: &Path, name: &str) -> bool {
    let samples = root.join("samples");
    let Ok(entries) = std::fs::read_dir(&samples) else {
        return false;
    };
    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        manifests.push(dir.join("Cargo.toml"));
        if let Ok(inner) = std::fs::read_dir(&dir) {
            manifests.extend(inner.flatten().map(|e| e.path().join("Cargo.toml")));
        }
    }
    manifests.iter().any(|manifest| {
        let package = std::fs::read_to_string(manifest)
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok());
        package
            .as_ref()
            .and_then(|value| value.get("package")?.get("name")?.as_str())
            == Some(name)
    })
}

// ---------------------------------------------------------------------------
// CLI commands
// ---------------------------------------------------------------------------

/// One variant of a clap subcommand enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliVariant {
    /// The variant's identifier as written in the source (`Mode2d`).
    pub ident: String,
    /// The subcommand name as clap spells it.
    pub name: String,
    /// The type named in the variant's payload (`commands::steam::SteamCommand` gives
    /// `SteamCommand`; `Box<X>` gives `X`), when it is a one-element tuple.
    pub payload: Option<String>,
    /// Whether the variant carries `#[command(subcommand)]`.
    pub nested: bool,
}

/// The top-level subcommand names of `enum CliCommand` in `regia_cli`'s `main.rs`, as clap names
/// them: the variant in kebab-case, or the `name = ".."` of its `#[command(..)]` attribute.
pub(crate) fn cli_commands(source: &str) -> Result<Vec<String>, String> {
    Ok(enum_variants(source, "CliCommand")?
        .into_iter()
        .map(|v| v.name)
        .collect())
}

/// The variants of `enum <name>` in `source`.
pub(crate) fn enum_variants(source: &str, name: &str) -> Result<Vec<CliVariant>, String> {
    let at = item_start(source, "enum", name).ok_or_else(|| format!("regia_cli: no `enum {name}`"))?;
    let brace = source[at..]
        .find('{')
        .ok_or_else(|| format!("regia_cli: `enum {name}` has no body"))?;
    let start = at + brace + 1;
    let end = match_end(source, start).ok_or_else(|| format!("regia_cli: `enum {name}` is not closed"))?;
    let body = &source[start..end];
    let bytes = body.as_bytes();
    let mut variants = Vec::new();
    let mut i = 0;
    let mut rename: Option<String> = None;
    let mut nested = false;
    while i < body.len() {
        i = skip_ws_and_comments(body, i);
        if i >= body.len() {
            break;
        }
        if body[i..].starts_with("#[") {
            let close = attr_close(body, i).ok_or_else(|| "regia_cli: unclosed attribute".to_string())?;
            let attr = &body[i..close];
            if attr.starts_with("#[command") {
                rename = attr_name_value(attr).or(rename);
                nested |= attr.contains("subcommand");
            }
            i = close;
            continue;
        }
        let Some((ident, next)) = read_ident(body, i) else {
            i += utf8_len(body, i);
            continue;
        };
        let variant_name = rename.take().unwrap_or_else(|| kebab_case(ident));
        let payload_start = next;
        // Skip the variant's payload up to the next top-level comma.
        let mut depth = 0i32;
        let mut j = next;
        let mut payload_end = body.len();
        while j < body.len() {
            if let Some(e) = skip_comment(body, j) {
                j = e;
                continue;
            }
            if let Some((e, _)) = take_string(body, j) {
                j = e;
                continue;
            }
            match bytes[j] {
                b'(' | b'{' | b'[' | b'<' => depth += 1,
                b')' | b'}' | b']' | b'>' => depth -= 1,
                b',' if depth == 0 => {
                    payload_end = j;
                    j += 1;
                    break;
                }
                _ => {}
            }
            j += utf8_len(body, j);
        }
        variants.push(CliVariant {
            ident: ident.to_string(),
            name: variant_name,
            payload: tuple_payload_type(&body[payload_start..payload_end.min(body.len())]),
            nested: std::mem::take(&mut nested),
        });
        i = j;
    }
    if variants.is_empty() {
        return Err(format!("regia_cli: `enum {name}` has no variants"));
    }
    Ok(variants)
}

/// The type of a one-element tuple payload `(a::b::T)` / `(Box<a::T>)`, as its last path segment.
fn tuple_payload_type(payload: &str) -> Option<String> {
    let inner = payload.trim().strip_prefix('(')?.strip_suffix(')')?.trim();
    let inner = inner
        .strip_prefix("Box<")
        .and_then(|r| r.strip_suffix('>'))
        .unwrap_or(inner);
    let last = inner.rsplit("::").next()?.trim();
    (!last.is_empty() && last.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| last.to_string())
}

/// The offset just after `<kind> <name>` (`enum Foo`, `struct Foo`) in `source` where it is a
/// whole-word item header at the start of a line (after `pub`/`pub(crate)`), outside comments.
fn item_start(source: &str, kind: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    let needle = format!("{kind} {name}");
    while let Some(rel) = source[from..].find(&needle) {
        let at = from + rel;
        let after = at + needle.len();
        from = after;
        let boundary = source[after..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let line_start = source[..at].rfind('\n').map_or(0, |n| n + 1);
        let before = source[line_start..at].trim();
        let header = before.is_empty() || before.starts_with("pub");
        if boundary && header {
            return Some(after);
        }
    }
    None
}

/// The subcommand enum a struct names through a `#[command(subcommand)]` field
/// (`pub session: Option<TestSubcommand>` gives `TestSubcommand`), if it has one.
fn struct_subcommand_type(source: &str, name: &str) -> Option<String> {
    let at = item_start(source, "struct", name)?;
    let brace = source[at..].find('{')?;
    let start = at + brace + 1;
    let end = match_end(source, start)?;
    let body = &source[start..end];
    let mut from = 0;
    while let Some(rel) = body[from..].find("#[command") {
        let attr_at = from + rel;
        let close = attr_close(body, attr_at)?;
        from = close;
        if !body[attr_at..close].contains("subcommand") {
            continue;
        }
        let field = body[close..].trim_start();
        let (_, ty) = field.split_once(':')?;
        let ty = ty.split([',', '\n']).next()?.trim();
        let ty = ty
            .strip_prefix("Option<")
            .and_then(|r| r.strip_suffix('>'))
            .unwrap_or(ty);
        return ty.rsplit("::").next().map(|t| t.trim().to_string());
    }
    None
}

/// Every nested subcommand path (`steam upload`, `procgen graph check`) of the `regia` CLI, from
/// the `#[command(subcommand)]` variants of `enum CliCommand` and the enums they name, found among
/// `sources` (the text of every `regia_cli` source file). Top-level names are not included.
pub(crate) fn nested_cli_commands(main: &str, sources: &[String]) -> Result<Vec<String>, String> {
    fn find_enum<'a>(sources: &'a [String], name: &str) -> Option<&'a String> {
        sources.iter().find(|s| item_start(s, "enum", name).is_some())
    }
    fn walk(
        prefix: &str,
        enum_name: &str,
        main: &str,
        sources: &[String],
        depth: usize,
        out: &mut Vec<String>,
    ) -> Result<(), String> {
        if depth > 6 {
            return Ok(());
        }
        let source = if enum_name == "CliCommand" {
            main
        } else {
            find_enum(sources, enum_name)
                .map(String::as_str)
                .ok_or_else(|| format!("regia_cli: no `enum {enum_name}` in any source file"))?
        };
        for variant in enum_variants(source, enum_name)? {
            let path = if prefix.is_empty() {
                variant.name.clone()
            } else {
                format!("{prefix} {}", variant.name)
            };
            if !prefix.is_empty() {
                out.push(path.clone());
            }
            let Some(payload) = variant.payload.as_deref() else {
                continue;
            };
            if variant.nested {
                walk(&path, payload, main, sources, depth + 1, out)?;
            } else if let Some(inner) = sources.iter().find_map(|s| struct_subcommand_type(s, payload))
                && find_enum(sources, &inner).is_some()
            {
                walk(&path, &inner, main, sources, depth + 1, out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk("", "CliCommand", main, sources, 0, &mut out)?;
    for source in sources {
        out.extend(attached_subcommands(source));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Subcommands a `regia_cli` function attaches with clap's builder API instead of a derive enum: a
/// function that calls `find_subcommand_mut("<parent>")` and builds `Command::new("<child>")` adds
/// `<parent> <child>` (`regia export usda`, ADR-4604, attached so `commands/export.rs` stays as it is).
fn attached_subcommands(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_ident(source, |at, end| {
        if &source[at..end] != "fn" {
            return;
        }
        let Some(open) = source[end..].find('{').map(|o| end + o) else {
            return;
        };
        // A declaration without a body (a trait method, a `fn` pointer type) ends before it.
        if source[end..open].contains(';') {
            return;
        }
        let Some(close) = match_end(source, open + 1) else {
            return;
        };
        let body = &source[open + 1..close];
        let parents = literal_args(body, "find_subcommand_mut(");
        let children = literal_args(body, "Command::new(");
        for parent in &parents {
            for child in &children {
                out.push(format!("{parent} {child}"));
            }
        }
    });
    out
}

/// The string-literal first argument of every call `call` (written with its `(`) in `text`.
fn literal_args(text: &str, call: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find(call) {
        let arg = skip_ws_and_comments(text, from + found + call.len());
        if let Some((_, literal)) = take_string(text, arg) {
            out.push(literal);
        }
        from += found + call.len();
    }
    out
}

/// The end (one past `]`) of the attribute starting at `i`.
fn attr_close(text: &str, i: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut j = i + 1;
    while j < bytes.len() {
        if let Some((e, _)) = take_string(text, j) {
            j = e;
            continue;
        }
        match bytes[j] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += utf8_len(text, j);
    }
    None
}

/// The `name = ".."` value inside an attribute, when there is one.
fn attr_name_value(attr: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = attr[from..].find("name") {
        let at = from + rel;
        from = at + 4;
        if !is_keyword(attr, at, "name") {
            continue;
        }
        let eq = skip_ws_and_comments(attr, at + 4);
        if !attr[eq..].starts_with('=') {
            continue;
        }
        let value = skip_ws_and_comments(attr, eq + 1);
        if let Some((_, literal)) = take_string(attr, value) {
            return Some(literal);
        }
    }
    None
}

/// `InstallAndroid` → `install-android` (clap's default subcommand naming).
pub(crate) fn kebab_case(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 4);
    for (i, c) in ident.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The subcommand a CLI-table command cell documents: `regia cook [..]` gives `cook`. `None` for
/// cells that are not a `regia` subcommand (`regia-editor`, `cargo xtask ..`, `pak_migrate`).
pub(crate) fn cli_row_command(cell: &str) -> Option<String> {
    let code = backticked(cell).into_iter().next()?;
    let rest = code.strip_prefix("regia ")?;
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!word.is_empty()).then_some(word)
}

fn cli_findings(commands: &[String], rows: &[Vec<String>], pages: &mut PageCache) -> Vec<Finding> {
    let s = Section::Cli;
    let mut out = Vec::new();
    let parsed: Vec<(Option<String>, Vec<String>)> = rows
        .iter()
        .map(|cells| {
            let command = cli_row_command(cells.first().map_or("", String::as_str));
            let page = links(cells.last().map_or("", String::as_str))
                .into_iter()
                .filter(|t| is_book_link(t))
                .collect();
            (command, page)
        })
        .collect();
    for command in commands {
        let subject = format!("regia {command}");
        let own: Vec<_> = parsed
            .iter()
            .filter(|(c, _)| c.as_deref() == Some(command.as_str()))
            .collect();
        if own.is_empty() {
            out.push(finding(
                s,
                "missing_row",
                &subject,
                format!("CLI command `regia {command}` has no row in the book/COVERAGE.md CLI table"),
            ));
        }
        for (_, targets) in own {
            if targets.is_empty() {
                out.push(finding(
                    s,
                    "missing_page",
                    &subject,
                    format!("CLI command `regia {command}`: its row links no page"),
                ));
            }
            for target in targets {
                if pages.get(target).is_none() {
                    out.push(finding(
                        s,
                        "missing_page",
                        &subject,
                        format!("CLI command `regia {command}` maps to `{target}`, which does not exist"),
                    ));
                }
            }
        }
    }
    if !commands.is_empty() {
        for (command, _) in &parsed {
            if let Some(command) = command
                && !commands.contains(command)
            {
                out.push(finding(
                    s,
                    "stale_row",
                    format!("regia {command}"),
                    format!("the CLI table documents `regia {command}`, which is not a `regia` subcommand"),
                ));
            }
        }
    }
    out
}

/// Whether `word` can be a subcommand name in a table cell: lowercase letters, digits and `-`
/// (`list`, `input-manifest`), not a placeholder (`FILE`, `<id>`, `[PROJECT]`) or a flag.
fn is_command_word(word: &str) -> bool {
    word.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && word
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The subcommand words after `regia ` in one code span (`regia steam upload --branch …` gives
/// `steam`, `upload`). `None` when the span is not a `regia` command.
fn span_words(span: &str) -> Option<Vec<String>> {
    let rest = span.trim().strip_prefix("regia ")?;
    let words: Vec<String> = rest
        .split_whitespace()
        .take_while(|w| is_command_word(w))
        .map(str::to_string)
        .collect();
    (!words.is_empty()).then_some(words)
}

/// The command paths a CLI-table row documents, from its first cell. The first code span names a
/// path (`regia plugin list`); later spans of the same cell that start with a subcommand word
/// (`info <id>`, `check [--platform P]`) are siblings of it under the same parent.
fn row_command_paths(cell: &str) -> Vec<Vec<String>> {
    let spans = backticked(cell);
    let Some(first) = spans.first().and_then(|s| span_words(s)) else {
        return Vec::new();
    };
    let parent = first[..first.len() - 1].to_vec();
    let mut out = vec![first];
    if !parent.is_empty() {
        for span in &spans[1..] {
            if let Some(own) = span_words(span) {
                out.push(own);
                continue;
            }
            let word = span.split_whitespace().next().unwrap_or("");
            if is_command_word(word) {
                let mut path = parent.clone();
                path.push(word.to_string());
                out.push(path);
            }
        }
    }
    out
}

/// Nested subcommands (`steam upload`, `engine package`) against the CLI table: every path needs a
/// row that documents it (a row for `regia steam upload`, or a list row `regia plugin list`,
/// `info <id>`, ...), and a row that names a nested path under a command with subcommands must name
/// a real one. Top-level commands are [`cli_findings`]'s.
fn nested_cli_findings(top: &[String], nested: &[String], rows: &[Vec<String>]) -> Vec<Finding> {
    let s = Section::Cli;
    let mut out = Vec::new();
    let mut documented: BTreeSet<String> = BTreeSet::new();
    let mut named: Vec<Vec<String>> = Vec::new();
    for cells in rows {
        for path in row_command_paths(cells.first().map_or("", String::as_str)) {
            for end in 1..=path.len() {
                documented.insert(path[..end].join(" "));
            }
            named.push(path);
        }
    }
    for path in nested {
        if !documented.contains(path) {
            out.push(finding(
                s,
                "missing_row",
                format!("regia {path}"),
                format!("CLI subcommand `regia {path}` has no row in the book/COVERAGE.md CLI table"),
            ));
        }
    }
    let known: BTreeSet<&str> = nested
        .iter()
        .map(String::as_str)
        .chain(top.iter().map(String::as_str))
        .collect();
    let with_children: BTreeSet<&str> = nested
        .iter()
        .filter_map(|p| p.split_once(' ').map(|(first, _)| first))
        .collect();
    for path in named {
        if path.len() < 2 || !with_children.contains(path[0].as_str()) {
            continue;
        }
        let joined = path.join(" ");
        if !known.contains(joined.as_str()) {
            out.push(finding(
                s,
                "stale_row",
                format!("regia {joined}"),
                format!("the CLI table documents `regia {joined}`, which is not a `regia` subcommand"),
            ));
        }
    }
    out
}

/// The text of every `.rs` file under `crates/regia_cli/src`.
pub(crate) fn cli_sources(root: &Path) -> Result<Vec<String>> {
    let dir = root.join("crates/regia_cli/src");
    let mut files = Vec::new();
    if dir.is_dir() {
        walk_rs(&dir, &mut files)?;
    }
    files.sort();
    files
        .iter()
        .map(|f| std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display())))
        .collect()
}

// ---------------------------------------------------------------------------
// Content formats
// ---------------------------------------------------------------------------

/// The committed registry's shape (only the parts read here).
#[derive(serde::Deserialize)]
struct Registry {
    formats: BTreeMap<String, RegistryEntry>,
}

/// One registry record.
#[derive(serde::Deserialize)]
struct RegistryEntry {
    version: u32,
}

/// Every format of [`FORMAT_REGISTRY`] and its version. Empty when the tree has no registry (a
/// book fixture).
pub fn registry_formats(root: &Path) -> Result<BTreeMap<String, u32>> {
    let path = root.join(FORMAT_REGISTRY);
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let registry: Registry = ron::from_str(&text).with_context(|| format!("parsing {FORMAT_REGISTRY}"))?;
    Ok(registry
        .formats
        .into_iter()
        .map(|(name, entry)| (name, entry.version))
        .collect())
}

/// Whether a format-table cell names registry format `name`: the name itself, or with `-` as `_`
/// or dropped (`render-job` is named by `.render_job.ron`, `post-volume` by `postvolume`), as a
/// whole token.
fn names_format(cell: &str, name: &str) -> bool {
    [name.to_string(), name.replace('-', "_"), name.replace('-', "")]
        .iter()
        .any(|variant| regia_agent::mentions_token(cell, variant))
}

fn format_findings(
    formats: &BTreeMap<String, u32>,
    rows: &[Vec<String>],
    pages: &mut PageCache,
) -> Vec<Finding> {
    let s = Section::Format;
    let mut out = Vec::new();
    for name in formats.keys() {
        let own: Vec<&Vec<String>> = rows
            .iter()
            .filter(|cells| cells.iter().take(2).any(|cell| names_format(cell, name)))
            .collect();
        if own.is_empty() {
            out.push(finding(
                s,
                "missing_row",
                name,
                format!(
                    "frozen format `{name}` ({FORMAT_REGISTRY}) has no row in the book/COVERAGE.md content-format table naming it in its Format or Extension cell"
                ),
            ));
        }
        for cells in own {
            let targets: Vec<String> = links(cells.last().map_or("", String::as_str))
                .into_iter()
                .filter(|t| is_book_link(t))
                .collect();
            if targets.is_empty() {
                out.push(finding(
                    s,
                    "missing_page",
                    name,
                    format!("frozen format `{name}`: its row links no page"),
                ));
            }
            for target in targets {
                if pages.get(&target).is_none() {
                    out.push(finding(
                        s,
                        "missing_page",
                        name,
                        format!("frozen format `{name}` maps to `{target}`, which does not exist"),
                    ));
                }
            }
        }
    }
    if !formats.is_empty() {
        for cells in rows {
            for token in claimed_format_names(cells) {
                if !formats
                    .keys()
                    .any(|name| format_name_variants(name).contains(&token))
                {
                    out.push(finding(
                        s,
                        "stale_row",
                        &token,
                        format!(
                            "the content-format table names format `{token}` in the row for {}, which {FORMAT_REGISTRY} does not register",
                            cells.first().map_or("?", String::as_str)
                        ),
                    ));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Editor panels
// ---------------------------------------------------------------------------

/// The variant identifiers of `enum Panel` in the editor shell's source.
pub(crate) fn panel_variants(source: &str) -> Result<Vec<String>, String> {
    Ok(enum_variants(source, "Panel")?
        .into_iter()
        .map(|v| v.ident)
        .collect())
}

/// `target` (a link in the panel guide, `a.md#x` or `../b.md`) as a path relative to `book/`.
fn panel_page_path(target: &str) -> String {
    let path = target.split_once('#').map_or(target, |(path, _)| path);
    let mut parts: Vec<&str> = PANEL_GUIDE_DIR.split('/').collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Every `Panel` variant has a row in the editor guide's panel table whose page exists, or a row in
/// the table of panels with no page of their own with a reason. A row for a variant that is not in
/// the enum is stale; a variant in both tables is ambiguous.
fn panel_findings(
    variants: &[String],
    rows: &[Vec<String>],
    exempt: &[Vec<String>],
    pages: &mut PageCache,
) -> Vec<Finding> {
    let s = Section::Panel;
    let variant_of = |cells: &Vec<String>| {
        backticked(cells.first().map_or("", String::as_str))
            .into_iter()
            .next()
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for variant in variants {
        let own: Vec<&Vec<String>> = rows.iter().filter(|c| &variant_of(c) == variant).collect();
        let reasons: Vec<&Vec<String>> = exempt.iter().filter(|c| &variant_of(c) == variant).collect();
        if own.is_empty() && reasons.is_empty() {
            out.push(finding(
                s,
                "missing_row",
                variant,
                format!(
                    "`Panel::{variant}` has no row in the panel table of {PANEL_GUIDE}: add one that links its page, or list it in the table of panels with no page of their own with a reason"
                ),
            ));
        }
        if !own.is_empty() && !reasons.is_empty() {
            out.push(finding(
                s,
                "duplicate_row",
                variant,
                format!("`Panel::{variant}` is in both panel tables of {PANEL_GUIDE}"),
            ));
        }
        for cells in own {
            let targets: Vec<String> = links(cells.last().map_or("", String::as_str))
                .into_iter()
                .filter(|t| is_book_link(t))
                .collect();
            if targets.is_empty() {
                out.push(finding(
                    s,
                    "missing_page",
                    variant,
                    format!("`Panel::{variant}`: its row in {PANEL_GUIDE} links no page"),
                ));
            }
            for target in targets {
                if pages.get(&panel_page_path(&target)).is_none() {
                    out.push(finding(
                        s,
                        "missing_page",
                        variant,
                        format!("`Panel::{variant}` maps to `{target}`, which does not exist"),
                    ));
                }
            }
        }
        for cells in reasons {
            if cells.get(1).is_none_or(|reason| reason.trim().is_empty()) {
                out.push(finding(
                    s,
                    "empty_reason",
                    variant,
                    format!("`Panel::{variant}` has no page and gives no reason in {PANEL_GUIDE}"),
                ));
            }
        }
    }
    for cells in rows.iter().chain(exempt) {
        let named = variant_of(cells);
        if !variants.contains(&named) {
            out.push(finding(
                s,
                "stale_row",
                &named,
                format!(
                    "{PANEL_GUIDE} lists `{named}`, which is not a variant of `enum Panel` in {PANEL_SOURCE}"
                ),
            ));
        }
    }
    out
}

/// Method areas and names: a `` `area.method` `` token in a § 4 row whose area belongs to the
/// protocol but whose method does not exist is stale, and so is a `regia_x::..` path whose crate
/// is not in the workspace.
fn stale_reference_findings(root: &Path, rows: &[Vec<String>], methods: &BTreeSet<String>) -> Vec<Finding> {
    const FILE_EXTENSIONS: [&str; 12] = [
        "ron", "toml", "json", "txt", "bin", "md", "rs", "csv", "wgsl", "meta", "key", "pem",
    ];
    let areas: BTreeSet<&str> = methods
        .iter()
        .filter_map(|m| m.split_once('.').map(|(a, _)| a))
        .collect();
    let mut out = Vec::new();
    for cells in rows {
        let label = cells.first().map_or("?", String::as_str);
        for token in cells.iter().flat_map(|c| backticked(c)) {
            let mut rest = token.as_str();
            while let Some(at) = rest.find("regia_") {
                let name: String = rest[at..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                let after = &rest[at + name.len()..];
                let starts_word =
                    at == 0 || !rest[..at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
                if starts_word
                    && (after.starts_with("::") || at == 0 && after.is_empty())
                    && !root.join("crates").join(&name).join("Cargo.toml").is_file()
                    && !root.join("samples").join(&name).join("Cargo.toml").is_file()
                {
                    out.push(finding(
                        Section::Format,
                        "stale_row",
                        &name,
                        format!("the content-format table row for {label} names crate `{name}`, which is not in the workspace"),
                    ));
                }
                rest = &rest[at + name.len().max(1)..];
            }
            let plain = |part: &str| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            };
            if let Some((area, method)) = token.split_once('.')
                && plain(area)
                && plain(method)
                && areas.contains(area)
                && !FILE_EXTENSIONS.contains(&method)
                && !methods.contains(&token)
            {
                out.push(finding(
                    Section::Format,
                    "stale_row",
                    &token,
                    format!("the content-format table row for {label} names agent method `{token}`, which nothing registers"),
                ));
            }
        }
    }
    out
}

/// A name and its two other spellings (`render-job`, `render_job`, `renderjob`).
fn format_name_variants(name: &str) -> [String; 3] {
    [name.to_string(), name.replace('-', "_"), name.replace('-', "")]
}

/// The registry name a § 4 row claims: a backticked token in parentheses in its Format cell
/// (`Data table (`data-table`)`). Free text elsewhere is not a claim (diagnostic outputs such as
/// the trace recording carry versions without being registered formats).
pub(crate) fn claimed_format_names(cells: &[String]) -> Vec<String> {
    fn plain(token: &str) -> bool {
        !token.is_empty()
            && token
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    }
    let mut out = Vec::new();
    if let Some(cell) = cells.first() {
        for part in cell.split("(`").skip(1) {
            if let Some((token, _)) = part.split_once('`')
                && plain(token)
            {
                out.push(token.to_string());
            }
        }
    }
    out
}

/// Example editor tool manifests the repository ships (`*.tool.toml` outside test fixtures, target
/// directories and dot directories). The dynamic `tool.<id>.<name>` methods come from such a
/// manifest, so a shipped one needs its methods covered by hand (spec 96 § 11, ADR-2648); the check
/// reports each so the exclusion cannot go stale silently.
pub(crate) fn shipped_tool_manifests(root: &Path) -> Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if name == "target" || name.starts_with('.') || name == "tests" || name == "fixtures" {
                    continue;
                }
                walk(root, &path, out)?;
            } else if name.ends_with(".tool.toml") {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    for top in [
        "examples",
        "assets",
        "config",
        "templates",
        "crates",
        "tools",
        "samples",
    ] {
        walk(root, &root.join(top), &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// A finding per shipped editor tool manifest (see [`shipped_tool_manifests`]).
fn tool_manifest_findings(manifests: &[String]) -> Vec<Finding> {
    manifests
        .iter()
        .map(|m| {
            finding(
                Section::Extension,
                "shipped_tool_unchecked",
                m,
                format!(
                    "`{m}` is a shipped editor tool manifest: its `tool.<id>.<name>` methods are not checked; add a row and a page for each (spec 96 § 11, ADR-2648)"
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_schema_scan_takes_the_top_level_name_only() {
        let src = r#"
            // MethodSchema { name: "in.comment" }
            const S: &str = "MethodSchema { name: \"in.string\" }";
            fn a() -> MethodSchema {
                MethodSchema {
                    params: &[ParamSchema { name: "nested", kind: "string" }],
                    name: "editor.select",
                    tool: "select",
                }
            }
            pub struct MethodSchema { pub name: &'static str }
            let c = '{';
            let b = MethodSchema { name: "steam.status", ..x };
        "#;
        assert_eq!(method_schema_names(src), ["editor.select", "steam.status"]);
    }

    #[test]
    fn constructor_calls_are_schema_rows() {
        let src = r#"
            const fn m(name: &'static str, tool: &'static str) -> MethodSchema {
                MethodSchema { name, tool, params: &[] }
            }
            fn other(name: &str) -> String { name.to_string() }
            fn rows() -> Vec<MethodSchema> {
                vec![m("editor.anim_open", "anim_open"), m(
                    "editor.anim_save", "anim_save"), self.m("not.a_call", "x")]
            }
            fn unrelated() { other("not.schema"); }
        "#;
        assert_eq!(schema_constructors(src).into_iter().collect::<Vec<_>>(), ["m"]);
        assert_eq!(method_schema_names(src), ["editor.anim_open", "editor.anim_save"]);
    }

    #[test]
    fn cfg_test_items_are_blanked() {
        let src = "fn a() { MethodSchema { name: \"a.shipped\" }; }\n#[cfg(test)]\nmod tests {\n    fn t() { MethodSchema { name: \"a.test_only\" }; }\n}\n";
        let spans = test_spans(src);
        let blanked = blank_spans(src, &spans);
        assert_eq!(blanked.len(), src.len());
        assert_eq!(method_schema_names(&blanked), ["a.shipped"]);
    }

    #[test]
    fn table_rows_keep_escaped_pipes_and_skip_the_separator() {
        let text = "intro\n\n| Command | What it does | Page |\n|---|---|---|\n| `regia new [blank\\|fps]` | New. | [a](a.md) |\n| `regia run` | Run. | [b](b.md), [c](c.md#x) |\n\nafter\n| `regia orphan` | Not in a table. | [d](d.md) |\n\n| Command | What it does | Page |\n|---|---|---|\n| `regia cook` | Cook. | [e](e.md) |\n";
        let rows = table_rows(text, &CLI_HEADER);
        assert_eq!(rows.len(), 3, "both tables, not the orphan row: {rows:?}");
        assert_eq!(rows[2][0], "`regia cook`");
        assert_eq!(rows[0][0], "`regia new [blank|fps]`");
        assert_eq!(links(&rows[1][2]), ["b.md", "c.md#x"]);
        assert!(table_rows(text, &EXTENSION_HEADER).is_empty());
    }

    #[test]
    fn cli_enum_scan_kebab_cases_and_honors_renames() {
        let src = r#"
            enum Other { Nope }
            #[derive(Subcommand)]
            enum CliCommand {
                /// Doc with a comma, and (parens).
                #[command(subcommand)]
                Agent(AgentCommand),
                InstallAndroid(Args<'static, X>),
                #[command(name = "bake-gi2")]
                BakeGi(Box<Args>),
                Doctor { verbose: bool, level: u8 },
                New,
            }
        "#;
        assert_eq!(
            cli_commands(src).expect("scan"),
            ["agent", "install-android", "bake-gi2", "doctor", "new"]
        );
        assert!(cli_commands("enum CliCommand {}").is_err());
    }

    #[test]
    fn cli_row_cells_name_their_subcommand() {
        assert_eq!(
            cli_row_command("`regia cook [--platform …]`").as_deref(),
            Some("cook")
        );
        assert_eq!(cli_row_command("`regia run`").as_deref(), Some("run"));
        assert_eq!(
            cli_row_command("`regia steam upload --branch …`").as_deref(),
            Some("steam")
        );
        assert_eq!(cli_row_command("`regia-editor [--project DIR]`"), None);
        assert_eq!(cli_row_command("`cargo xtask book-check`"), None);
    }

    #[test]
    fn manifest_features_are_sorted_keys() {
        let manifest = "[package]\nname = \"x\"\n\n[features]\ndefault = [\"b\"]\nb = []\na = [\"dep:y\"]\n\n[dependencies]\n";
        assert_eq!(manifest_features(manifest).expect("toml"), ["a", "b", "default"]);
        assert!(
            manifest_features("[package]\nname = \"x\"\n")
                .expect("toml")
                .is_empty()
        );
    }

    #[test]
    fn backticks_and_kebab_case() {
        assert_eq!(
            backticked("`default` = `agent`, `ui`"),
            ["default", "agent", "ui"]
        );
        assert_eq!(kebab_case("UpgradeProject"), "upgrade-project");
        assert_eq!(kebab_case("New"), "new");
    }

    fn methods(list: &[(&str, &str)]) -> BTreeMap<String, String> {
        list.iter()
            .map(|(m, c)| ((*m).to_string(), (*c).to_string()))
            .collect()
    }

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| (*c).to_string()).collect()
    }

    fn codes(findings: &[Finding]) -> Vec<(&'static str, String)> {
        let mut out: Vec<_> = findings.iter().map(|f| (f.code, f.subject.clone())).collect();
        out.sort();
        out
    }

    #[test]
    fn each_extension_finding_code_fires() {
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-ext");
        std::fs::create_dir_all(dir.join("agents")).expect("mkdir");
        std::fs::write(dir.join("agents/ed.md"), "`editor.select` and `editor.other`").expect("write");
        let mut pages = PageCache::new(&dir);
        let schema: BTreeSet<&str> = ["scene.spawn"].into_iter().collect();
        let tested: BTreeSet<String> = ["editor.select".to_string()].into_iter().collect();
        let found = extension_findings(
            &methods(&[
                ("editor.select", "regia_editor"),
                ("editor.undocumented", "regia_editor"),
                ("editor.wrong_page", "regia_editor"),
                ("steam.status", "regia_steam"),
            ]),
            &schema,
            &[
                row(&["`editor.select`", "`regia_editor`", "[ed](agents/ed.md)"]),
                row(&["`editor.wrong_page`", "`regia_editor`", "[ed](agents/ed.md)"]),
                row(&["`steam.status`", "`regia_editor`", "[st](agents/missing.md)"]),
                row(&["`scene.spawn`", "`regia_agent`", "[ed](agents/ed.md)"]),
                row(&["`editor.gone`", "`regia_editor`", "[ed](agents/ed.md)"]),
            ],
            &tested,
            &mut pages,
        );
        assert_eq!(
            codes(&found),
            [
                ("missing_page", "steam.status".to_string()),
                ("missing_row", "editor.undocumented".to_string()),
                ("page_does_not_mention", "editor.wrong_page".to_string()),
                ("stale_row", "editor.gone".to_string()),
                ("untested", "editor.undocumented".to_string()),
                ("untested", "editor.wrong_page".to_string()),
                ("untested", "steam.status".to_string()),
                ("wrong_registrar", "steam.status".to_string()),
                ("wrong_section", "scene.spawn".to_string()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn feature_and_cli_findings() {
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-feat");
        std::fs::create_dir_all(dir.join("book/user-guide")).expect("mkdir");
        std::fs::create_dir_all(dir.join("crates/regia_empty")).expect("mkdir");
        std::fs::create_dir_all(dir.join("samples/hot/fixture")).expect("mkdir");
        std::fs::write(
            dir.join("samples/hot/fixture/Cargo.toml"),
            "[package]\nname = \"hot_fixture\"\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("crates/regia_empty/Cargo.toml"),
            "[package]\nname = \"regia_empty\"\n",
        )
        .expect("write");
        std::fs::write(dir.join("book/user-guide/a.md"), "# A").expect("write");
        let mut pages = PageCache::new(&dir.join("book"));
        let features: BTreeMap<String, Vec<String>> = [(
            "regia".to_string(),
            vec!["agent".to_string(), "net".to_string(), "ui".to_string()],
        )]
        .into_iter()
        .collect();
        let (found, notes) = feature_findings(
            &features,
            &[
                row(&["`regia`", "`agent`, `ui`", "x", "[A](user-guide/a.md)"]),
                row(&["`regia`", "`net`", "x", "[B](user-guide/b.md)"]),
                row(&["`regia`", "`modules`", "x", "[A](user-guide/a.md)"]),
                row(&["`regia_empty`", "`x`", "x", "[A](user-guide/a.md)"]),
                row(&[
                    "`hot_fixture` (sample)",
                    "`hot-reload`",
                    "x",
                    "[A](user-guide/a.md)",
                ]),
                row(&["`regia_gone`", "`x`", "x", "[A](user-guide/a.md)"]),
            ],
            &dir,
            &mut pages,
        );
        assert_eq!(
            codes(&found),
            [
                ("missing_page", "regia/net".to_string()),
                ("stale_row", "regia_gone".to_string()),
            ]
        );
        assert_eq!(
            notes.len(),
            2,
            "the modules row and the featureless crate: {notes:?}"
        );

        let commands = vec!["cook".to_string(), "run".to_string()];
        let found = cli_findings(
            &commands,
            &[
                row(&["`regia cook [--x]`", "Cook.", "[A](user-guide/a.md)"]),
                row(&["`regia gone`", "Old.", "[A](user-guide/a.md)"]),
                row(&["`cargo xtask book-check`", "Check.", "[A](user-guide/a.md)"]),
            ],
            &mut pages,
        );
        assert_eq!(
            codes(&found),
            [
                ("missing_row", "regia run".to_string()),
                ("stale_row", "regia gone".to_string()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_rows_name_registry_formats() {
        assert!(names_format("`.render_job.ron`", "render-job"));
        assert!(names_format("Post volume (`postvolume`)", "post-volume"));
        assert!(names_format("`<slot>.save` (`save`, v1)", "save"));
        assert!(!names_format("`.savegame.ron`", "save"));
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-formats");
        std::fs::create_dir_all(dir.join("docs/formats")).expect("mkdir");
        std::fs::write(
            dir.join(FORMAT_REGISTRY),
            "(\n    formats: {\n        \"render-job\": (\n            version: 1,\n            hash: \"ab\",\n        ),\n        \"save\": (version: 2, hash: \"cd\"),\n    },\n)\n",
        )
        .expect("write");
        let formats = registry_formats(&dir).expect("registry");
        assert_eq!(formats.get("render-job"), Some(&1));
        assert_eq!(formats.get("save"), Some(&2));
        std::fs::create_dir_all(dir.join("book/agents")).expect("mkdir");
        std::fs::write(dir.join("book/agents/render-jobs.md"), "# Jobs").expect("write");
        let mut pages = PageCache::new(&dir.join("book"));
        let found = format_findings(
            &formats,
            &[
                row(&[
                    "Render job",
                    "`.render_job.ron`",
                    "x",
                    "[Jobs](agents/render-jobs.md)",
                ]),
                row(&[
                    "Save game (described, not named)",
                    "`.sav`",
                    "x",
                    "[Jobs](agents/render-jobs.md)",
                ]),
            ],
            &mut pages,
        );
        assert_eq!(codes(&found), [("missing_row", "save".to_string())]);
        assert!(registry_formats(&dir.join("book")).expect("none").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_report_shape() {
        let report = Report {
            extension_methods: methods(&[("editor.select", "regia_editor")]),
            features: [("regia".to_string(), vec!["agent".to_string()])]
                .into_iter()
                .collect(),
            cli_commands: vec!["cook".into()],
            cli_nested: vec!["steam upload".into()],
            formats: [("save".to_string(), 1)].into_iter().collect(),
            findings: vec![finding(Section::Cli, "missing_row", "regia cook", "m".into())],
            notes: Vec::new(),
            fences: FenceCounts {
                compiled: 3,
                ignored: 1,
                ignored_by_page: [("user-guide/a.md".to_string(), 1)].into_iter().collect(),
                unjustified_by_page: [("user-guide/a.md".to_string(), 1)].into_iter().collect(),
            },
        };
        let value = report.to_json();
        assert_eq!(value["format_version"], REPORT_FORMAT_VERSION);
        assert_eq!(value["ok"], false);
        assert_eq!(value["counts"]["extension_methods"], 1);
        assert_eq!(value["counts"]["features"], 1);
        assert_eq!(value["counts"]["formats"], 1);
        assert_eq!(value["findings"][0]["section"], "cli");
        assert_eq!(value["findings"][0]["code"], "missing_row");
        assert_eq!(value["extension_methods"]["editor.select"], "regia_editor");
        assert_eq!(value["fences"]["compiled"], 3);
        assert_eq!(value["fences"]["ignored_by_page"]["user-guide/a.md"], 1);
        assert_eq!(value["fences"]["unjustified_by_page"]["user-guide/a.md"], 1);
        assert_eq!(value["counts"]["cli_nested_commands"], 1);
    }

    #[test]
    fn fences_are_counted_per_page() {
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-fences");
        std::fs::create_dir_all(dir.join("guide")).expect("mkdir");
        std::fs::write(
            dir.join("guide/a.md"),
            "```rust\nfn a() {}\n```\n```rust,ignore\nlet x;\n```\n```rust,no_run\nfn b() {}\n```\n```json\n{}\n```\n",
        )
        .expect("write");
        std::fs::write(dir.join("b.md"), "```rust,ignore\nx\n```\n").expect("write");
        let counts = fence_counts(&dir).expect("count");
        assert_eq!((counts.compiled, counts.ignored), (2, 2));
        assert_eq!(counts.ignored_by_page.get("guide/a.md"), Some(&1));
        assert_eq!(counts.ignored_by_page.get("b.md"), Some(&1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignore_reasons_are_read_from_the_comment_before_the_fence() {
        let text = "<!-- ignore-reason: needs a GPU -->\n```rust,ignore\nx\n```\n\n<!-- ignore-reason: -->\n```rust,ignore\ny\n```\ntext\n```rust,ignore\nz\n```\n";
        let fences = fences_with_reason(text);
        assert_eq!(
            fences,
            [
                ("rust,ignore".to_string(), true),
                ("rust,ignore".to_string(), false),
                ("rust,ignore".to_string(), false)
            ]
        );
        let blank_between = "<!-- ignore-reason: why -->\n\n\n```rust,ignore\nx\n```\n";
        assert!(
            fences_with_reason(blank_between)[0].1,
            "blank lines between are fine"
        );
        let inside = "```text\n<!-- ignore-reason: why -->\n```\n```rust,ignore\nx\n```\n";
        assert!(
            !fences_with_reason(inside)[1].1,
            "a comment inside another fence does not count"
        );
    }

    #[test]
    fn justified_ignores_do_not_count_against_the_baseline() {
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-reasons");
        std::fs::write(
            dir.join("a.md"),
            "<!-- ignore-reason: needs a device -->\n```rust,ignore\nx\n```\n```rust,ignore\ny\n```\n",
        )
        .expect("write");
        let counts = fence_counts(&dir).expect("count");
        assert_eq!(counts.ignored, 2);
        assert_eq!(counts.unjustified_by_page.get("a.md"), Some(&1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn counts(pages: &[(&str, usize)]) -> FenceCounts {
        FenceCounts {
            unjustified_by_page: pages.iter().map(|(p, n)| ((*p).to_string(), *n)).collect(),
            ..FenceCounts::default()
        }
    }

    #[test]
    fn the_ratchet_fails_on_growth_and_notes_a_drop() {
        let baseline: BTreeMap<String, usize> = [("a.md".to_string(), 2), ("gone.md".to_string(), 1)]
            .into_iter()
            .collect();
        // At the baseline, and a new page at 0: clean.
        let (findings, notes) = fence_findings(&counts(&[("a.md", 2)]), &baseline);
        assert!(findings.is_empty());
        assert_eq!(notes.len(), 1, "gone.md fell below its entry: {notes:?}");
        // One more than the baseline.
        let (findings, _) = fence_findings(&counts(&[("a.md", 3)]), &baseline);
        assert_eq!(
            codes(&findings),
            [("ignored_fence_grew", "book/a.md".to_string())]
        );
        // A page with no entry starts at 0.
        let (findings, _) = fence_findings(&counts(&[("new.md", 1)]), &baseline);
        assert_eq!(
            codes(&findings),
            [("ignored_fence_grew", "book/new.md".to_string())]
        );
        // Lower than the baseline: only a note.
        let (findings, notes) = fence_findings(&counts(&[("a.md", 1), ("gone.md", 1)]), &baseline);
        assert!(findings.is_empty());
        assert!(notes[0].contains("a.md"));
    }

    #[test]
    fn the_baseline_file_round_trips_and_rejects_bad_shapes() {
        let text = fence_baseline_text(&counts(&[("b.md", 1), ("a/x.md", 4)]));
        let parsed = parse_fence_baseline(&text).expect("parse");
        assert_eq!(parsed.get("a/x.md"), Some(&4));
        assert_eq!(parsed.get("b.md"), Some(&1));
        assert!(text.find("a/x.md") < text.find("b.md"), "sorted");
        assert!(parse_fence_baseline("{\"format_version\": 2, \"ignored_by_page\": {}}").is_err());
        assert!(
            parse_fence_baseline("{\"format_version\": 1, \"ignored_by_page\": {\"a.md\": -1}}").is_err()
        );
        assert!(parse_fence_baseline("{\"format_version\": 1}").is_err());
        let empty = fence_baseline_text(&FenceCounts::default());
        assert!(parse_fence_baseline(&empty).expect("empty").is_empty());
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-baseline");
        assert!(read_fence_baseline(&dir).expect("absent").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const NESTED_MAIN: &str = r#"
        enum AgentCommand { Call(CallArgs) }
        enum CliCommand {
            #[command(subcommand)]
            Agent(AgentCommand),
            #[command(subcommand)]
            Steam(commands::steam::SteamCommand),
            #[command(subcommand)]
            Procgen(ProcgenCommand),
            #[command(subcommand)]
            #[command(name = "dcc")]
            DccFeed(DccFeedCommand),
            Test(commands::test::TestArgs),
            Cook(CookArgs),
            Run(Box<RunArgs>),
        }
    "#;

    fn nested_sources() -> Vec<String> {
        vec![
            NESTED_MAIN.to_string(),
            "pub enum SteamCommand {\n    /// Upload.\n    Upload(UploadArgs),\n    InputManifest(M),\n}".to_string(),
            "pub enum ProcgenCommand {\n    #[command(subcommand)]\n    Graph(GraphCommand),\n    Plant(P),\n}\npub enum GraphCommand { Check(C) }".to_string(),
            "pub enum DccFeedCommand { Stream }".to_string(),
            "pub struct TestArgs {\n    /// `session`.\n    #[command(subcommand)]\n    pub session: Option<TestSubcommand>,\n    #[arg(long)]\n    pub level: String,\n}\npub enum TestSubcommand { Session(S) }".to_string(),
            "struct CookArgs { #[arg(long)] pub clean: bool }".to_string(),
        ]
    }

    #[test]
    fn nested_subcommands_follow_enums_and_struct_fields() {
        let nested = nested_cli_commands(NESTED_MAIN, &nested_sources()).expect("scan");
        assert_eq!(
            nested,
            [
                "agent call",
                "dcc stream",
                "procgen graph",
                "procgen graph check",
                "procgen plant",
                "steam input-manifest",
                "steam upload",
                "test session",
            ]
        );
        assert!(
            nested_cli_commands(NESTED_MAIN, &[NESTED_MAIN.to_string()]).is_err(),
            "SteamCommand is missing"
        );
    }

    /// `regia export usda` is attached with clap's builder API (`find_subcommand_mut("export")` plus
    /// `Command::new("usda")`), not a derive enum: it is a nested command, so its CLI row is not stale.
    #[test]
    fn builder_attached_subcommands_are_nested_commands() {
        let attach = r#"
            /// Attaches `usda`.
            pub fn attach(command: &mut clap::Command) {
                let export = command
                    .find_subcommand_mut("export")
                    .expect("regia export is a command");
                let usda = clap::Command::new("usda").about("Write one USDA stage");
                *export = std::mem::take(export).subcommand(usda);
            }
            fn unrelated() -> clap::Command { clap::Command::new("other") }
            trait T { fn declared(&self); }
        "#;
        assert_eq!(attached_subcommands(attach), ["export usda"]);
        let mut sources = nested_sources();
        sources.push(attach.to_string());
        let nested = nested_cli_commands(NESTED_MAIN, &sources).expect("scan");
        assert!(nested.iter().any(|p| p == "export usda"), "{nested:?}");
        assert!(!nested.iter().any(|p| p.ends_with(" other")), "{nested:?}");
        // Without the attaching function the derive scan alone does not know it.
        let derive_only = nested_cli_commands(NESTED_MAIN, &nested_sources()).expect("scan");
        assert!(!derive_only.iter().any(|p| p == "export usda"));
    }

    #[test]
    fn row_cells_name_command_paths() {
        let paths =
            |cell: &str| -> Vec<String> { row_command_paths(cell).iter().map(|p| p.join(" ")).collect() };
        assert_eq!(paths("`regia steam upload --branch …`"), ["steam upload"]);
        assert_eq!(paths("`regia test session FILE [--recover]`"), ["test session"]);
        assert_eq!(paths("`regia test [PROJECT] --level L`"), ["test"]);
        assert_eq!(
            paths("`regia plugin list`, `info <id>`, `check [--platform P]`, `--json`"),
            ["plugin list", "plugin info", "plugin check"]
        );
        assert_eq!(paths("`cargo xtask book-check`"), Vec::<String>::new());
    }

    #[test]
    fn nested_cli_findings_fire_for_missing_and_stale_rows() {
        let top = vec!["steam".to_string(), "plugin".to_string(), "cook".to_string()];
        let nested = vec![
            "steam input-manifest".to_string(),
            "steam upload".to_string(),
            "plugin list".to_string(),
        ];
        let rows = vec![
            row(&["`regia steam upload --branch …`", "Upload.", "[S](s.md)"]),
            row(&["`regia steam sign`", "Gone.", "[S](s.md)"]),
            row(&["`regia plugin list`, `gone <id>`", "x", "[P](p.md)"]),
            row(&["`regia cook --clean`", "no nested commands", "[C](c.md)"]),
        ];
        assert_eq!(
            codes(&nested_cli_findings(&top, &nested, &rows)),
            [
                ("missing_row", "regia steam input-manifest".to_string()),
                ("stale_row", "regia plugin gone".to_string()),
                ("stale_row", "regia steam sign".to_string()),
            ]
        );
        let all = vec![
            row(&["`regia steam upload`", "x", "[S](s.md)"]),
            row(&["`regia steam input-manifest`", "x", "[S](s.md)"]),
            row(&["`regia plugin list`", "x", "[P](p.md)"]),
        ];
        assert!(nested_cli_findings(&top, &nested, &all).is_empty());
    }

    #[test]
    fn format_rows_claim_registry_names() {
        let claims = |cells: &[&str]| claimed_format_names(&row(cells));
        assert_eq!(
            claims(&["Data table (`data-table`)", ".table.ron", "x", "p"]),
            ["data-table"]
        );
        assert!(
            claims(&["Trace", "`.regiatrace` (`regiatrace` v1; diagnostic)", "x", "p"]).is_empty(),
            "an Extension-cell version is not a registry claim"
        );
        assert!(claims(&["Level", "`.level.ron`", "x", "p"]).is_empty());
        assert!(claims(&["Net (`NetSettings`)", "config (`format_version` 1)", "x", "p"]).is_empty());
        let formats = methods(&[]);
        assert!(formats.is_empty());
        let registry: BTreeMap<String, u32> = [("data-table".to_string(), 1), ("save".to_string(), 1)]
            .into_iter()
            .collect();
        let rows = vec![
            row(&["Data table (`data-table`)", ".table.ron", "x", "[p](p.md)"]),
            row(&["Cooked thing (`cooked-thing`)", ".thing.bin", "x", "[p](p.md)"]),
            row(&["Save", "`save` v1", "x", "[p](p.md)"]),
        ];
        let mut pages = PageCache::new(Path::new("/nonexistent-book"));
        let findings = format_findings(&registry, &rows, &mut pages);
        let stale: Vec<_> = findings
            .iter()
            .filter(|f| f.code == "stale_row")
            .map(|f| f.subject.clone())
            .collect();
        assert_eq!(stale, ["cooked-thing"]);
    }

    #[test]
    fn shipped_tool_manifests_are_reported_outside_fixtures() {
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-tools");
        std::fs::create_dir_all(dir.join("examples/demo/tools")).expect("mkdir");
        std::fs::create_dir_all(dir.join("crates/x/tests")).expect("mkdir");
        std::fs::write(dir.join("examples/demo/tools/lint.tool.toml"), "id = \"lint\"\n").expect("write");
        std::fs::write(dir.join("crates/x/tests/fixture.tool.toml"), "id = \"f\"\n").expect("write");
        let manifests = shipped_tool_manifests(&dir).expect("scan");
        assert_eq!(manifests, ["examples/demo/tools/lint.tool.toml"]);
        let findings = tool_manifest_findings(&manifests);
        assert_eq!(findings[0].code, "shipped_tool_unchecked");
        assert!(
            shipped_tool_manifests(&dir.join("empty"))
                .expect("none")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sample projects ship to users like the examples, so a tool manifest there is reported too (spec 96 § 11
    /// exempts only tests, fixtures, target and dot directories; ADR-5660: `samples/` used to be skipped).
    #[test]
    fn sample_projects_are_scanned_for_tool_manifests() {
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-sample-tools");
        std::fs::create_dir_all(dir.join("samples/lab/tools")).expect("mkdir");
        std::fs::create_dir_all(dir.join("samples/lab/fixtures")).expect("mkdir");
        std::fs::write(dir.join("samples/lab/tools/paint.tool.toml"), "id = \"paint\"\n").expect("write");
        std::fs::write(dir.join("samples/lab/fixtures/f.tool.toml"), "id = \"f\"\n").expect("write");
        assert_eq!(
            shipped_tool_manifests(&dir).expect("scan"),
            ["samples/lab/tools/paint.tool.toml"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_rows_with_dead_crates_or_methods_are_stale() {
        let root = regia_test_utils::temp::unique_temp_dir("book-coverage-stale-refs");
        std::fs::create_dir_all(root.join("crates/regia_live")).expect("mkdir");
        std::fs::write(
            root.join("crates/regia_live/Cargo.toml"),
            "[package]\nname = \"regia_live\"\n",
        )
        .expect("write");
        let methods: BTreeSet<String> = ["render.get_lightmaps", "editor.select"]
            .iter()
            .map(|m| (*m).to_string())
            .collect();
        let rows = vec![
            row(&[
                "Live",
                "`regia_live::panel::Panel`, `render.get_lightmaps`, `render.ron`",
                "x",
                "p",
            ]),
            row(&["Dead crate", "`regia_gone::thing::Thing`", "x", "p"]),
            row(&["Dead method", "`render.get_nothing`, `editor.select`", "x", "p"]),
            row(&["Unknown area", "`gameplay.thing`", "x", "p"]),
            row(&["Prose", "regia_gone in text, `my_regia_gone::x`", "x", "p"]),
        ];
        let findings = stale_reference_findings(&root, &rows, &methods);
        assert_eq!(
            codes(&findings),
            [
                ("stale_row", "regia_gone".to_string()),
                ("stale_row", "render.get_nothing".to_string())
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Spec 96 § 8 acceptance on the real tree: dropping any one row of the § 5 extension table gives exactly one
    /// finding, `missing_row` naming that method (`book-check` prints the same findings, so it fails too). The test-call
    /// side is held fixed here; `the_workspace_book_covers_every_shipped_surface` checks it on the tree.
    #[test]
    fn removing_any_one_extension_row_names_that_method() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent");
        let coverage = std::fs::read_to_string(root.join("book/COVERAGE.md")).expect("COVERAGE.md");
        let schema: BTreeSet<&str> = regia_agent::schema::METHODS.iter().map(|m| m.name).collect();
        let methods = extension_methods(root, &schema).expect("scan");
        let tested: BTreeSet<String> = methods.keys().cloned().collect();
        let rows = table_rows(&coverage, &EXTENSION_HEADER);
        let mut pages = PageCache::new(&root.join("book"));
        let all = extension_findings(&methods, &schema, &rows, &tested, &mut pages);
        assert!(all.is_empty(), "the full table has findings: {:?}", codes(&all));
        let method_of = |cells: &Vec<String>| {
            backticked(cells.first().map_or("", String::as_str))
                .into_iter()
                .next()
                .unwrap_or_default()
        };
        let mut checked = 0usize;
        for (i, removed) in rows.iter().enumerate() {
            let method = method_of(removed);
            // A method with a second row stays covered by it.
            if rows.iter().filter(|r| method_of(r) == method).count() > 1 {
                continue;
            }
            let rest: Vec<Vec<String>> = rows
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, r)| r.clone())
                .collect();
            let found = extension_findings(&methods, &schema, &rest, &tested, &mut pages);
            assert_eq!(codes(&found), [("missing_row", method.clone())], "row {i}");
            checked += 1;
        }
        assert!(checked > 0, "no extension rows in book/COVERAGE.md");
    }

    #[test]
    fn panel_variants_are_read_as_written() {
        let source = "/// The panel.\npub enum Panel {\n    /// Viewport.\n    Viewport,\n    /// 2D `mode`.\n    Mode2d,\n    /// Tuple.\n    ExtensionPanel(&'static str, &'static str),\n}\nenum Other { Nope }\n";
        assert_eq!(
            panel_variants(source).expect("parse"),
            ["Viewport", "Mode2d", "ExtensionPanel"]
        );
    }

    #[test]
    fn each_panel_finding_code_fires() {
        let dir = regia_test_utils::temp::unique_temp_dir("book-coverage-panel");
        std::fs::create_dir_all(dir.join("user-guide/editor-guide")).expect("mkdir");
        std::fs::write(dir.join("user-guide/editor-guide/a.md"), "# A").expect("write");
        std::fs::write(dir.join("user-guide/b.md"), "# B").expect("write");
        let mut pages = PageCache::new(&dir);
        let variants: Vec<String> = [
            "Fine", "Up", "NoRow", "NoLink", "Dead", "Both", "NoReason", "Skipped",
        ]
        .iter()
        .map(|v| (*v).to_string())
        .collect();
        let found = panel_findings(
            &variants,
            &[
                row(&["`Fine`", "Fine", "[A](a.md)"]),
                row(&["`Up`", "Up", "[B](../b.md#x)"]),
                row(&["`NoLink`", "No link", "none"]),
                row(&["`Dead`", "Dead", "[X](gone.md)"]),
                row(&["`Both`", "Both", "[A](a.md)"]),
                row(&["`Gone`", "Gone", "[A](a.md)"]),
            ],
            &[
                row(&["`Both`", "debug only"]),
                row(&["`NoReason`", " "]),
                row(&["`Skipped`", "internal: draws the dock's drag preview"]),
                row(&["`AlsoGone`", "internal"]),
            ],
            &mut pages,
        );
        assert_eq!(
            codes(&found),
            [
                ("duplicate_row", "Both".to_string()),
                ("empty_reason", "NoReason".to_string()),
                ("missing_page", "Dead".to_string()),
                ("missing_page", "NoLink".to_string()),
                ("missing_row", "NoRow".to_string()),
                ("stale_row", "AlsoGone".to_string()),
                ("stale_row", "Gone".to_string()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real tree: every `Panel` variant maps to a guide page or a reasoned exemption, and
    /// removing any one row names that variant.
    #[test]
    fn every_panel_variant_has_a_guide_page() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent");
        let source = std::fs::read_to_string(root.join(PANEL_SOURCE)).expect("shell.rs");
        let guide = std::fs::read_to_string(root.join(PANEL_GUIDE)).expect("editor guide index");
        let variants = panel_variants(&source).expect("Panel variants");
        assert!(variants.len() > 40, "only {} variants read", variants.len());
        let rows = table_rows(&guide, &PANEL_HEADER);
        let exempt = table_rows(&guide, &PANEL_EXEMPT_HEADER);
        let mut pages = PageCache::new(&root.join("book"));
        let all = panel_findings(&variants, &rows, &exempt, &mut pages);
        assert!(all.is_empty(), "panel findings: {:?}", codes(&all));
        let variant_of = |cells: &Vec<String>| {
            backticked(cells.first().map_or("", String::as_str))
                .into_iter()
                .next()
                .unwrap_or_default()
        };
        for (i, removed) in rows.iter().enumerate() {
            let rest: Vec<Vec<String>> = rows
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, r)| r.clone())
                .collect();
            let found = panel_findings(&variants, &rest, &exempt, &mut pages);
            assert_eq!(codes(&found), [("missing_row", variant_of(removed))], "row {i}");
        }
    }
}
