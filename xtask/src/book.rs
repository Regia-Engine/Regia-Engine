// SPDX-License-Identifier: MPL-2.0
//! `book-check`: verify the user and agent guide under `book/`.
//!
//! Seven checks, each a hard failure:
//!
//! 1. **Dead links** — every `[text](target)` and `![alt](target)` in a `.md` file resolves to a
//!    file that exists in the book and every local fragment resolves to one heading anchor.
//! 2. **Schema coverage** — every method of the agent protocol (`regia_agent::schema::METHODS`)
//!    is mapped, in `book/COVERAGE.md`, to an agent-guide page that exists and mentions the
//!    method as a whole token (`assets.load_progress` does not mention `assets.load`). A page for a method that is not in the schema is also an error (the map must not
//!    drift).
//! 3. **Summary completeness** — every `.md` file in the book is reachable from `book/SUMMARY.md`.
//! 4. **Agent coverage** — when `crates/regia_agent/src/dispatch.rs` exists, every method that
//!    dispatcher registers also has a schema entry, an `agent.guide` topic, a coverage row, a
//!    guide-page mention and a test call ([`crate::agent_coverage`]). A book fixture with no
//!    dispatcher file skips this check.
//! 5. **RON fences** — every named RON fence parses through the real loader of its format.
//! 6. **Rust fences** — every bare `rust` fence compiles in a throwaway crate (mark a fragment
//!    `rust,ignore`); a compile error names the page and block.
//! 7. **Book coverage** — with the dispatcher present, host extension methods, Cargo features and
//!    `regia` subcommands each have a `book/COVERAGE.md` row and a page ([`crate::book_coverage`],
//!    spec 96).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;

const ANCHOR_BASELINE: &str = "anchor-baseline.json";

/// The book directory relative to the workspace root.
pub const BOOK_DIR: &str = "book";

/// The coverage map (`book/COVERAGE.md`).
pub const COVERAGE: &str = "COVERAGE.md";

/// The summary (`book/SUMMARY.md`).
pub const SUMMARY: &str = "SUMMARY.md";

/// Collects every `.md` file under `book/`, as a path relative to `book/`.
pub(crate) fn book_markdown(book: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk(book, book, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

/// Extracts every markdown link target: the text inside `](…)` (covers `[x](…)` and `![x](…)`).
fn link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b')' {
                j += 1;
            }
            if j < bytes.len() {
                targets.push(text[start..j].to_string());
                i = j;
            }
        }
        i += 1;
    }
    targets
}

/// Whether a link target is external (never resolved against the book tree).
fn is_external(target: &str) -> bool {
    target.is_empty()
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.contains("://")
}

/// Strips a `#fragment` from a target, returning the path part.
fn strip_fragment(target: &str) -> &str {
    target.split_once('#').map_or(target, |(path, _)| path)
}

/// One error message from the dead-link check.
fn dead_link_errors(book: &Path, files: &[PathBuf]) -> Vec<String> {
    let mut errors = Vec::new();
    for rel in files {
        let path = book.join(rel);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(err) => {
                errors.push(format!("{}: cannot read: {err}", rel.display()));
                continue;
            }
        };
        let parent = path.parent().unwrap_or(book);
        for target in link_targets(&text) {
            let target = target.trim();
            if is_external(target) {
                continue;
            }
            let path_part = strip_fragment(target);
            let target_page = if path_part.is_empty() {
                Some(path.clone())
            } else {
                let target_path = parent.join(path_part);
                target_path.is_file().then_some(target_path)
            };
            let Some(target_page) = target_page else {
                errors.push(format!("{}: dead link to `{target}`", rel.display()));
                continue;
            };
            if let Some((_, fragment)) = target.split_once('#') {
                let target_text = if target_page == path {
                    Some(text.clone())
                } else {
                    std::fs::read_to_string(&target_page).ok()
                };
                let anchors = target_text.as_deref().map(heading_anchors);
                let fragment = fragment.trim();
                let valid = anchors
                    .as_ref()
                    .is_some_and(|anchors| anchors.get(fragment).is_some_and(|count| *count == 1));
                if !valid {
                    errors.push(format!("{}\0{}", rel.display(), target));
                }
            }
        }
    }
    errors
}

#[derive(serde::Deserialize)]
struct AnchorBaseline {
    format_version: u32,
    rows: Vec<AnchorBaselineRow>,
}

#[derive(serde::Deserialize)]
struct AnchorBaselineRow {
    page: String,
    target: String,
    reason: String,
}

fn anchor_baseline_errors(book: &Path, anchor_misses: &[String]) -> Result<Vec<String>> {
    let baseline: AnchorBaseline = serde_json::from_slice(&std::fs::read(book.join(ANCHOR_BASELINE))?)?;
    let mut errors = Vec::new();
    if baseline.format_version != 1 {
        errors.push(format!(
            "{ANCHOR_BASELINE}: unsupported format_version {}",
            baseline.format_version
        ));
        return Ok(errors);
    }
    let expected = baseline
        .rows
        .iter()
        .map(|row| format!("{}\0{}", row.page, row.target))
        .collect::<BTreeSet<_>>();
    let actual = anchor_misses.iter().cloned().collect::<BTreeSet<_>>();
    for miss in actual.difference(&expected) {
        let (page, target) = miss.split_once('\0').unwrap_or((miss, ""));
        errors.push(format!(
            "{page}: unresolved or ambiguous anchor in `{target}` (add a baseline row with a reason)"
        ));
    }
    for stale in expected.difference(&actual) {
        let (page, target) = stale.split_once('\0').unwrap_or((stale, ""));
        errors.push(format!(
            "{page}: stale anchor baseline row for `{target}`; remove the row"
        ));
    }
    for row in &baseline.rows {
        if row.reason.trim().is_empty() {
            errors.push(format!(
                "{ANCHOR_BASELINE}: {} `{}` has no reason",
                row.page, row.target
            ));
        }
    }
    Ok(errors)
}

fn heading_anchors(text: &str) -> BTreeMap<String, usize> {
    let mut anchors = BTreeMap::new();
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        let heading = line.trim_start();
        let indentation = line.len() - heading.len();
        let marker = heading.chars().next();
        let marker_len = marker.map_or(0, |marker| heading.chars().take_while(|ch| *ch == marker).count());
        if let Some((fence_marker, fence_len)) = fence {
            if marker == Some(fence_marker)
                && marker_len >= fence_len
                && heading[marker_len..].trim().is_empty()
            {
                fence = None;
            }
            continue;
        }
        if indentation <= 3
            && marker_len >= 3
            && let Some(marker @ ('`' | '~')) = marker
        {
            fence = Some((marker, marker_len));
            continue;
        }
        if indentation > 3 {
            continue;
        }
        if !heading.starts_with('#') {
            continue;
        }
        // All the leading `#`s mark the level; the title follows a space.
        let rest = heading.trim_start_matches('#');
        let level = heading.len() - rest.len();
        if !(1..=6).contains(&level) || !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let title = rest.trim().trim_end_matches('#').trim();
        if title.is_empty() {
            continue;
        }
        let slug = crate::manual::slug(title);
        *anchors.entry(slug).or_insert(0) += 1;
    }
    anchors
}

/// A row of the `book/COVERAGE.md` method table: a method name and the agent page that documents
/// it (a path relative to `book/`).
pub(crate) struct CoverageRow {
    /// The method name.
    pub(crate) method: String,
    /// The agent page, relative to `book/`.
    pub(crate) page: String,
}

/// Parses the method table of `book/COVERAGE.md`.
///
/// The table has a header row whose cells are named `Method` and `Agent page`; each data row maps
/// `` `method.name` `` to a `[label](path)` link. The function returns `(rows, errors)` where
/// `errors` lists malformed rows.
pub(crate) fn coverage_table(text: &str) -> (Vec<CoverageRow>, Vec<String>) {
    let mut errors = Vec::new();
    let mut rows = Vec::new();

    let lines: Vec<&str> = text.lines().collect();
    let header = lines.iter().position(|l| {
        let cells = split_row(l);
        cells.len() >= 2 && cells.iter().any(|c| c == "Method") && cells.iter().any(|c| c == "Agent page")
    });

    let Some(header_idx) = header else {
        return (
            rows,
            vec![
                "COVERAGE.md has no method table (expected a header with `Method` and `Agent page` columns)"
                    .into(),
            ],
        );
    };
    let header_cells = split_row(lines[header_idx]);
    let method_col = header_cells.iter().position(|c| c == "Method");
    let page_col = header_cells.iter().position(|c| c == "Agent page");

    for line in lines.iter().skip(header_idx + 1) {
        if !line.trim_start().starts_with('|') {
            if rows.is_empty() {
                continue;
            }
            break;
        }
        let cells = split_row(line);
        // Skip the `|---|---|---|` separator row under the header.
        if cells.iter().all(|c| c.chars().all(|ch| ch == '-' || ch == ':')) {
            continue;
        }
        let method = method_col
            .and_then(|i| cells.get(i))
            .map(|c| c.trim_matches('`').to_string());
        let page = page_col
            .and_then(|i| cells.get(i))
            .and_then(|c| link_targets(c).into_iter().next());
        match (method, page) {
            (Some(method), Some(page)) if !method.is_empty() && !page.is_empty() => {
                rows.push(CoverageRow { method, page });
            }
            _ => errors.push(format!("COVERAGE.md: malformed method-table row: {line}")),
        }
    }
    (rows, errors)
}

/// Splits a markdown table row into trimmed cells (dropping the leading/trailing `|`).
fn split_row(line: &str) -> Vec<String> {
    line.split('|')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every schema method and the agent-guide page (relative to `book/`) that mentions it.
fn method_pages(book: &Path) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let agents_dir = book.join("agents");
    if agents_dir.is_dir() {
        for entry in std::fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let rel = path
                .strip_prefix(book)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            for method in regia_agent::schema::METHODS {
                // As a whole token: `assets.load_progress` is not a mention of `assets.load`.
                if regia_agent::mentions_token(&text, method.name) {
                    map.entry(method.name.to_string())
                        .or_default()
                        .insert(rel.clone());
                }
            }
        }
    }
    Ok(map)
}

/// Verifies the coverage map and agent pages cover the schema exactly.
fn schema_coverage_errors(book: &Path) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    let coverage_path = book.join(COVERAGE);
    let text = std::fs::read_to_string(&coverage_path)?;
    let (rows, parse_errors) = coverage_table(&text);
    errors.extend(parse_errors);

    let pages = method_pages(book)?;

    let schema: BTreeSet<&str> = regia_agent::schema::METHODS.iter().map(|m| m.name).collect();

    // Every schema method must be mapped to exactly one page that mentions it.
    for name in &schema {
        let rows_for = rows.iter().filter(|r| r.method == *name).collect::<Vec<_>>();
        if rows_for.is_empty() {
            errors.push(format!("schema method `{name}` has no row in COVERAGE.md"));
            continue;
        }
        for row in rows_for {
            let page_path = row.page.trim_start_matches("./");
            if !book.join(page_path).is_file() {
                errors.push(format!("`{name}` maps to `{page_path}`, which does not exist"));
                continue;
            }
            let mentioned = pages
                .get(*name)
                .is_some_and(|files| files.iter().any(|f| f == page_path));
            if !mentioned {
                errors.push(format!(
                    "`{name}` is mapped to `{page_path}` but that page does not mention it"
                ));
            }
        }
    }

    // Every mapped row must name a real schema method or a built-in extension method (ADR-1809;
    // `agent-coverage` checks those rows' pages, guide topics and tests).
    let extensions: BTreeSet<&str> = regia_agent::builtin_extension_methods().into_iter().collect();
    for row in &rows {
        if !schema.contains(row.method.as_str()) && !extensions.contains(row.method.as_str()) {
            errors.push(format!(
                "COVERAGE.md maps `{}` to `{}`, but `{}` is not a schema method",
                row.method, row.page, row.method
            ));
        }
    }

    // A schema method may also be mentioned on other pages, but never zero pages.
    for name in &schema {
        if !pages.contains_key(*name) {
            errors.push(format!(
                "schema method `{name}` is mentioned on no agent-guide page"
            ));
        }
    }

    Ok(errors)
}

/// Every `.md` file under `book/` that is not linked from `SUMMARY.md`.
fn summary_gaps(book: &Path, files: &[PathBuf]) -> Result<Vec<String>> {
    let summary = book.join(SUMMARY);
    let text = std::fs::read_to_string(&summary)?;
    let linked: BTreeSet<String> = link_targets(&text)
        .into_iter()
        .map(|t| strip_fragment(t.trim()).replace('\\', "/"))
        .filter(|t| !t.is_empty())
        .collect();
    let mut gaps = Vec::new();
    for rel in files {
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel == SUMMARY {
            continue;
        }
        if !linked.contains(&rel) {
            gaps.push(format!("{} is not linked from SUMMARY.md", rel));
        }
    }
    Ok(gaps)
}

/// One `rust` code block, with its page and position for error reporting.
struct RustBlock {
    /// Page path relative to `book/`.
    file: PathBuf,
    /// Zero-based index of the block within its page.
    index: usize,
    /// The code, verbatim.
    code: String,
}

/// Whether a fenced block's info string marks it as compilable Rust.
pub(crate) fn is_rust(info: &str) -> bool {
    let first = info.split(',').next().unwrap_or("").trim();
    first == "rust" && !info.contains("ignore")
}

/// Every fenced code block as `(info string, body)`, in document order.
pub(crate) fn fenced_blocks(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut info = String::new();
    let mut buf = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !in_fence {
            if let Some(rest) = trimmed.strip_prefix("```") {
                in_fence = true;
                info = rest.trim().to_string();
                buf.clear();
            }
        } else if trimmed.starts_with("```") {
            in_fence = false;
            out.push((std::mem::take(&mut info), std::mem::take(&mut buf)));
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    out
}

/// Whether a fence's info string asks for the body to be wrapped in a function (`rust,wrap`): the
/// page shows plain statements, and the compile check puts them in a function returning `Result`.
pub(crate) fn is_wrapped(info: &str) -> bool {
    is_rust(info) && info.split(',').skip(1).any(|t| t.trim() == "wrap")
}

/// The compile text of a hidden line: mdBook hides a line that starts with `# ` (or is a lone
/// `#`) when it renders a `rust` fence, and still compiles it. `#[..]`, `#![..]` and `##` are
/// ordinary text.
fn unhide_line(line: &str) -> String {
    let indent = line.len() - line.trim_start().len();
    let (lead, rest) = line.split_at(indent);
    if rest == "#" {
        lead.to_string()
    } else if let Some(shown) = rest.strip_prefix("# ") {
        format!("{lead}{shown}")
    } else if let Some(shown) = rest.strip_prefix("##") {
        format!("{lead}#{shown}")
    } else {
        line.to_string()
    }
}

/// The text the snippet crate compiles for one fence: hidden lines shown, and a `rust,wrap` body
/// inside a `fn` returning `Result<(), Box<dyn Error>>` (so `?` works; the body must end in a
/// statement).
fn compile_text(info: &str, body: &str) -> String {
    let unhidden: String = body.lines().map(|l| format!("{}\n", unhide_line(l))).collect();
    if is_wrapped(info) {
        format!("fn book_wrapped() -> Result<(), Box<dyn std::error::Error>> {{\n{unhidden}    Ok(())\n}}\n")
    } else {
        unhidden
    }
}

/// Extracts the compile text of every compilable `rust` fenced block from `text`, in order.
fn fenced_rust_blocks(text: &str) -> Vec<String> {
    fenced_blocks(text)
        .into_iter()
        .filter(|(info, _)| is_rust(info))
        .map(|(info, body)| compile_text(&info, &body))
        .collect()
}

/// Collects every `rust` block in the book.
fn rust_blocks(book: &Path, files: &[PathBuf]) -> Result<Vec<RustBlock>> {
    let mut blocks = Vec::new();
    for rel in files {
        let text = std::fs::read_to_string(book.join(rel))?;
        for (index, code) in fenced_rust_blocks(&text).into_iter().enumerate() {
            blocks.push(RustBlock {
                file: rel.clone(),
                index,
                code,
            });
        }
    }
    Ok(blocks)
}

/// Keywords that may begin an item. Every compiled book fence becomes the body of a `mod`, so
/// anything else at its top level (a `let`, a call, a `...` placeholder) cannot compile.
const ITEM_KEYWORDS: &[&str] = &[
    "use", "mod", "fn", "struct", "enum", "impl", "trait", "type", "const", "static", "extern", "unsafe",
    "async", "union", "pub",
];

/// Whether a trimmed line can begin an item (or is an attribute or a comment).
fn starts_item(trimmed: &str) -> bool {
    if ["#[", "#![", "//", "/*", "macro_rules!"]
        .iter()
        .any(|p| trimmed.starts_with(p))
    {
        return true;
    }
    // A macro invocation (`path::to::mac!(..);`) is a valid item.
    let head = trimmed.split(['(', '{', '[', ' ']).next().unwrap_or("");
    if head.ends_with('!')
        && head
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '!'))
    {
        return true;
    }
    let first = trimmed
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    ITEM_KEYWORDS.contains(&first)
}

/// A fast, compile-free look at one bare `rust` fence: the first top-level line that cannot start
/// an item (a statement or a `...` placeholder), as `(1-based line, text)`. `None` when the fence
/// looks like a module body.
///
/// A heuristic, not a parser: braces inside string literals can fool it. `book-check` stays the
/// authority; this finds the common mistake (a statement fragment in a bare fence) in
/// milliseconds so `preflight` fails before the snippet crate compiles.
fn first_non_item_line(code: &str) -> Option<(usize, String)> {
    let mut depth: i32 = 0;
    let mut at_item_start = true;
    for (i, line) in code.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if depth == 0 && at_item_start && !starts_item(trimmed) {
            return Some((i + 1, trimmed.to_string()));
        }
        for c in trimmed.chars() {
            match c {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        depth = depth.max(0);
        // An item ends at a `;` or at the `}` that closes its last block; an attribute line
        // (`#[derive(..)]`) leaves the next line as part of the same item.
        at_item_start = depth == 0 && (trimmed.ends_with(';') || trimmed.ends_with('}'));
    }
    None
}

/// The bare `rust` fences under `book/` that cannot be a module body, as `book/<page> (rust
/// block #N): line L ...` problems. The fast pre-check behind `cargo xtask book-check --fences`.
pub fn fence_problems(root: &Path) -> Result<Vec<String>> {
    let book = root.join(BOOK_DIR);
    let files = book_markdown(&book)?;
    let mut problems = Vec::new();
    for block in rust_blocks(&book, &files)? {
        if let Some((line, text)) = first_non_item_line(&block.code) {
            problems.push(format!(
                "book/{} (rust block #{}): line {line} is not an item (`{text}`); mark a fragment ```rust,ignore",
                block.file.to_string_lossy().replace('\\', "/"),
                block.index,
            ));
        }
    }
    Ok(problems)
}

/// `cargo xtask book-check --fences`: only the fast fence pre-check.
pub fn run_fences(root: &Path) -> Result<std::process::ExitCode> {
    let problems = fence_problems(root)?;
    if problems.is_empty() {
        println!("book-check --fences: ok");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    for problem in &problems {
        eprintln!("error: {problem}");
    }
    eprintln!("book-check --fences: {} problem(s)", problems.len());
    Ok(std::process::ExitCode::FAILURE)
}

/// The `[dependencies]` of the generated snippet crate: the engine crates the book snippets use.
fn snippet_manifest(root: &Path) -> String {
    let root = root.to_string_lossy().replace('\\', "/");
    let mut manifest = String::from(
        "[package]\nname = \"book-snippets\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
         [workspace]\n\n[dependencies]\n",
    );
    for name in [
        "regia",
        "regia_scene",
        "regia_render",
        "regia_framework",
        "regia_input",
        "regia_audio",
        "regia_animation",
        "regia_physics",
        "regia_terrain",
        "regia_asset",
        "regia_app",
        "regia_agent",
        "regia_nav",
        "regia_ai",
        "regia_core",
        "regia_detsim",
        "regia_ecs",
        "regia_platform",
        "regia_ui",
        "regia_vfx",
        "regia_matgraph",
        "regia_net",
        "regia_media",
        "regia_asset_pipeline",
        "regia_test_utils",
        "regia_pick",
        // det-grid.md's tunnel layer example (S-02) names regia_world_geom::compat (ADR-8195).
        "regia_world_geom",
        // det-navmesh.md's host example names the deterministic navigation crate (ADR-8877).
        "regia_nav_det",
        "serde",
    ] {
        if name == "serde" {
            manifest.push_str("serde = { version = \"1\", features = [\"derive\"] }\n");
        } else if name == "regia" {
            manifest.push_str(&format!(
                "regia = {{ path = \"{root}/crates/regia\", features = [\"steam\", \"net\", \"media\"] }}\n"
            ));
        } else {
            manifest.push_str(&format!("{name} = {{ path = \"{root}/crates/{name}\" }}\n"));
        }
    }
    // Sample crates a book page's examples name: the capture-the-flag tutorial's examples build the
    // arena's match (GR-8.3; `samples/arena` also runs them as its doctests).
    manifest.push_str(&format!("arena = {{ path = \"{root}/samples/arena\" }}\n"));
    manifest
}

/// The `src/lib.rs` line numbers named by the compiler's `-->` location lines. rustc indents them
/// (`  --> src/lib.rs:12:5`), and prints `\` separators on Windows.
fn lib_rs_error_lines(stderr: &str) -> Vec<usize> {
    stderr
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            line.strip_prefix("--> src/lib.rs:")
                .or_else(|| line.strip_prefix("--> src\\lib.rs:"))
        })
        .filter_map(|rest| {
            rest.split(':')
                .next()
                .and_then(|n| n.trim().parse::<usize>().ok())
        })
        .collect()
}

/// Compiles every `rust` block as a throwaway crate; returns one error per non-compiling block.
fn rust_block_errors(root: &Path, blocks: &[RustBlock]) -> Result<Vec<String>> {
    if blocks.is_empty() {
        return Ok(Vec::new());
    }

    // Assemble the library source, recording each block's 1-based line range so cargo's
    // `--> src/lib.rs:LINE:COL` diagnostics can be mapped back to their page.
    let mut source = String::from(
        "#![allow(dead_code, unused_imports, unused_variables, unused_mut, unused_parens, unreachable_code)]\n\n",
    );
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(blocks.len());
    for (i, block) in blocks.iter().enumerate() {
        source.push_str(&format!(
            "// block {i}: {}\n",
            block.file.to_string_lossy().replace('\\', "/")
        ));
        source.push_str(&format!(
            "#[allow(dead_code, unused_imports, unused_variables, unused_mut, unreachable_code)]\nmod book_{i} {{\n"
        ));
        let start = source.lines().count() + 1;
        source.push_str(&block.code);
        if !block.code.ends_with('\n') {
            source.push('\n');
        }
        let end = source.lines().count();
        source.push_str("}\n\n");
        ranges.push((start, end));
    }

    let dir = root.join("target").join("book-check");
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("Cargo.toml"), snippet_manifest(root))?;
    std::fs::write(dir.join("src").join("lib.rs"), &source)?;

    // Reuse the workspace lockfile so the snippet crate resolves the exact same versions.
    let lock = root.join("Cargo.lock");
    if lock.is_file() {
        let _ = std::fs::copy(&lock, dir.join("Cargo.lock"));
    }

    let output = std::process::Command::new("cargo")
        .arg("check")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(root.join("target"))
        .arg("--quiet")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run `cargo check` for book snippets: {e}"))?;

    if output.status.success() {
        return Ok(Vec::new());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut errors = Vec::new();
    let mut attributed = BTreeSet::new();
    for line_no in lib_rs_error_lines(&stderr) {
        if let Some(idx) = ranges.iter().position(|(s, e)| line_no >= *s && line_no <= *e)
            && attributed.insert(idx)
        {
            errors.push(format!(
                "book/{} (rust block #{}): does not compile",
                blocks[idx].file.to_string_lossy().replace('\\', "/"),
                blocks[idx].index,
            ));
        }
    }
    if errors.is_empty() {
        // No line attribution (crate-level or link error): report the raw compiler output.
        errors.push(format!("book rust snippets do not compile:\n{}", stderr.trim()));
    }
    Ok(errors)
}

// ---------------------------------------------------------------------------
// RON format samples
// ---------------------------------------------------------------------------

/// A frozen content format whose `ron` fence in the book must parse through the real loader.
///
/// A fence names its format with a second info token, e.g. `` ```ron,material ``. Plain `` ```ron ``
/// fences are not validated (the format is ambiguous); only the frozen asset formats are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RonFormat {
    /// `.material.ron` — [`regia_asset::MaterialAssetRon::from_ron`].
    Material,
    /// `.ui.ron` — [`regia_ui::parse_ui_ron`].
    Ui,
    /// `.animgraph.ron` — [`regia_animation::AnimGraph::from_ron_str`].
    AnimGraph,
    /// `.vfx.ron` — [`regia_vfx::VfxSystemDef::from_ron`].
    Vfx,
    /// `.matgraph.ron` — [`regia_matgraph::MaterialGraph::from_ron`], then the type-checking
    /// compiler (a book graph must compile, not only parse).
    MatGraph,
}

impl RonFormat {
    /// The fence token that names the format (e.g. `material` in `` ```ron,material ``).
    fn name(self) -> &'static str {
        match self {
            Self::Material => "material",
            Self::Ui => "ui",
            Self::AnimGraph => "animgraph",
            Self::Vfx => "vfx",
            Self::MatGraph => "matgraph",
        }
    }

    /// Maps an info token to a format, or `None` when it is not a frozen format name.
    fn from_info(name: &str) -> Option<Self> {
        match name {
            "material" => Some(Self::Material),
            "ui" => Some(Self::Ui),
            "animgraph" => Some(Self::AnimGraph),
            "vfx" => Some(Self::Vfx),
            "matgraph" => Some(Self::MatGraph),
            _ => None,
        }
    }

    /// Parses `code` through the format's real loader, returning the loader's error text.
    fn parse(self, code: &str) -> Result<(), String> {
        match self {
            Self::Material => regia_asset::MaterialAssetRon::from_ron(code.as_bytes())
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Self::Ui => regia_ui::parse_ui_ron(code, "<book>", &regia_ui::UiMigrators::default())
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Self::AnimGraph => regia_animation::AnimGraph::from_ron_str(code)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Self::Vfx => regia_vfx::VfxSystemDef::from_ron(code)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Self::MatGraph => regia_matgraph::MaterialGraph::from_ron(code)
                .map_err(|e| e.to_string())
                .and_then(|graph| {
                    regia_matgraph::compile_unvalidated(&graph)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }),
        }
    }
}

/// One named-format `ron` code block.
struct RonBlock {
    /// Page path relative to `book/`.
    file: PathBuf,
    /// Zero-based index of the block within its page.
    index: usize,
    /// The named format.
    format: RonFormat,
    /// The code, verbatim.
    code: String,
}

/// Extracts `ron` fences that name a frozen format, as `(format_token, code)` in order.
fn fenced_ron_blocks(text: &str) -> Vec<(String, String)> {
    fenced_blocks(text)
        .into_iter()
        .filter_map(|(info, body)| {
            let first = info.split(',').next().unwrap_or("").trim();
            let format = info.split(',').nth(1).map(str::trim).unwrap_or("");
            if first == "ron" && RonFormat::from_info(format).is_some() {
                Some((format.to_string(), body))
            } else {
                None
            }
        })
        .collect()
}

/// Collects every named-format `ron` block in the book.
fn ron_blocks(book: &Path, files: &[PathBuf]) -> Result<Vec<RonBlock>> {
    let mut blocks = Vec::new();
    for rel in files {
        let text = std::fs::read_to_string(book.join(rel))?;
        for (index, (format, code)) in fenced_ron_blocks(&text).into_iter().enumerate() {
            blocks.push(RonBlock {
                file: rel.clone(),
                index,
                format: RonFormat::from_info(&format).expect("validated by fenced_ron_blocks"),
                code,
            });
        }
    }
    Ok(blocks)
}

/// Parses every named-format `ron` block through its real loader; returns one error per failure.
fn ron_block_errors(blocks: &[RonBlock]) -> Vec<String> {
    let mut errors = Vec::new();
    for block in blocks {
        if let Err(e) = block.format.parse(&block.code) {
            errors.push(format!(
                "book/{} (ron block #{} `{}`): does not parse: {e}",
                block.file.to_string_lossy().replace('\\', "/"),
                block.index,
                block.format.name(),
            ));
        }
    }
    errors
}

/// Runs the whole check: prints findings and returns the process exit code.
pub fn run(root: &Path) -> Result<std::process::ExitCode> {
    let book = root.join(BOOK_DIR);
    let files = book_markdown(&book)?;
    if files.is_empty() {
        eprintln!("error: no markdown files under {}", book.display());
        return Ok(std::process::ExitCode::FAILURE);
    }

    let link_errors = dead_link_errors(&book, &files);
    let anchor_misses = link_errors
        .iter()
        .filter(|error| error.contains('\0'))
        .cloned()
        .collect::<Vec<_>>();
    let mut errors = link_errors
        .into_iter()
        .filter(|error| !error.contains('\0'))
        .collect::<Vec<_>>();
    errors.extend(anchor_baseline_errors(&book, &anchor_misses)?);
    errors.extend(schema_coverage_errors(&book)?);
    errors.extend(summary_gaps(&book, &files)?);
    errors.extend(ron_block_errors(&ron_blocks(&book, &files)?));
    errors.extend(rust_block_errors(root, &rust_blocks(&book, &files)?)?);

    // The merge gate runs `book-check` and does not name `agent-coverage` on its own.
    // Hook the dispatcher check here so a registered method cannot land without its
    // schema, guide topic, coverage row and test. Fixture books have no dispatcher.
    let dispatch = root.join("crates/regia_agent/src/dispatch.rs");
    let mut covered = None;
    if dispatch.is_file() {
        match crate::agent_coverage::check(root) {
            Ok(report) if report.errors.is_empty() => covered = Some(report.methods),
            Ok(report) => {
                errors.extend(
                    report
                        .errors
                        .into_iter()
                        .map(|error| format!("agent-coverage: {error}")),
                );
            }
            Err(err) => errors.push(format!("agent-coverage: {err:#}")),
        }
        // ADR-7307: the editor's methods meet the same rules (from the editor method snapshot).
        if root.join(crate::editor_agent_coverage::SNAPSHOT_PATH).is_file() {
            match crate::editor_agent_coverage::check(root) {
                Ok(report) => errors.extend(
                    report
                        .errors
                        .into_iter()
                        .map(|error| format!("editor-agent-coverage: {error}")),
                ),
                Err(err) => errors.push(format!("editor-agent-coverage: {err:#}")),
            }
        }
        // Spec 96: host extension methods, Cargo features and CLI subcommands.
        match crate::book_coverage::check(root) {
            Ok(report) => errors.extend(
                report
                    .findings
                    .into_iter()
                    .map(|f| format!("book-coverage: [{}] {}", f.code, f.message)),
            ),
            Err(err) => errors.push(format!("book-coverage: {err:#}")),
        }
    }

    errors.extend(crate::agent_examples::book_check_errors(root));

    // Generated reference pages lag the code until `cargo xtask manual` runs; say so, without
    // failing lanes that changed the code but not the book.
    for page in crate::manual::stale_generated_pages(&book)? {
        eprintln!("warning: book/{page} is out of date; run `cargo xtask manual`");
    }

    if errors.is_empty() {
        if let Some(methods) = covered {
            println!("agent-coverage: ok ({methods} methods)");
        }
        println!("book-check: ok ({} pages)", files.len());
        Ok(std::process::ExitCode::SUCCESS)
    } else {
        for e in &errors {
            eprintln!("error: {e}");
        }
        eprintln!("book-check: {} problem(s)", errors.len());
        Ok(std::process::ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_book(pages: &[(&str, &str)]) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("book-anchor-fixture");
        std::fs::create_dir_all(&root).expect("create fixture root");
        for (name, text) in pages {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().expect("fixture parent"))
                .expect("create fixture directory");
            std::fs::write(path, text).expect("write fixture page");
        }
        root
    }

    fn fixture_errors(pages: &[(&str, &str)]) -> Vec<String> {
        let root = fixture_book(pages);
        let files = book_markdown(&root).expect("list fixture pages");
        let errors = dead_link_errors(&root, &files);
        std::fs::remove_dir_all(root).expect("remove fixture root");
        errors
    }

    #[test]
    fn same_page_anchor_must_resolve() {
        assert!(fixture_errors(&[("a.md", "# Hello World\n[ok](#hello-world)\n")]).is_empty());
        assert!(
            fixture_errors(&[("a.md", "# Hello World\n[bad](#missing)\n")])
                .iter()
                .any(|error| error.contains("#missing"))
        );
    }

    #[test]
    fn cross_page_anchor_must_resolve_and_file_must_exist() {
        assert!(
            fixture_errors(&[("a.md", "[ok](other.md#target)\n"), ("other.md", "# Target\n"),]).is_empty()
        );
        assert!(
            fixture_errors(&[("a.md", "[bad](other.md#missing)\n"), ("other.md", "# Target\n")])
                .iter()
                .any(|error| error.contains("#missing"))
        );
        let errors = fixture_errors(&[("a.md", "[bad](other.md#target)\n")]);
        assert!(errors.iter().any(|error| error.contains("dead link")));
        assert!(!fixture_errors(&[("a.md", "[bad](missing.md)\n")]).is_empty());
    }

    #[test]
    fn external_links_remain_unchecked() {
        assert!(
            fixture_errors(&[(
                "a.md",
                "[a](http://invalid/#x) [b](https://invalid/#x) [c](mailto:a@b#x)\n"
            )])
            .is_empty()
        );
    }

    #[test]
    fn duplicate_heading_slugs_are_ambiguous() {
        assert!(!fixture_errors(&[("a.md", "# Target\n# Target\n[x](#target)\n")]).is_empty());
    }

    #[test]
    fn fenced_code_headings_do_not_create_anchors() {
        assert!(
            !fixture_errors(&[("a.md", "~~~markdown\n# Code Example\n~~~\n[x](#code-example)\n",)])
                .is_empty()
        );
    }

    /// integ-v049a (ADR-5923): every heading level makes an anchor. Only `# ` headings were
    /// counted (`strip_prefix('#')` left `# Shore` for `## Shore`, which failed the space test),
    /// so a link to any `##`..`######` heading read as unresolved and went into the baseline.
    #[test]
    fn every_heading_level_makes_an_anchor() {
        let anchors = heading_anchors(
            "# Page\n\n## Shore\n\n### Warm discovery cache\n\n###### Deep\n\n####### Not a heading\n",
        );
        for slug in ["page", "shore", "warm-discovery-cache", "deep"] {
            assert_eq!(anchors.get(slug), Some(&1), "{slug}: {anchors:?}");
        }
        assert!(!anchors.contains_key("not-a-heading"), "{anchors:?}");
        // The same title at two levels is ambiguous (counted twice).
        assert_eq!(heading_anchors("## Errors\n### Errors\n").get("errors"), Some(&2));
    }

    #[test]
    fn anchor_baseline_rejects_new_and_stale_misses() {
        let root = fixture_book(&[]);
        let baseline_path = root.join(ANCHOR_BASELINE);
        std::fs::write(&baseline_path, r#"{"format_version":1,"rows":[]}"#)
            .expect("write empty anchor baseline");
        let miss = "guide.md\0#missing".to_owned();
        let errors =
            anchor_baseline_errors(&root, std::slice::from_ref(&miss)).expect("read anchor baseline");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("unresolved or ambiguous"))
        );

        std::fs::write(
            &baseline_path,
            r##"{"format_version":1,"rows":[{"page":"guide.md","target":"#missing","reason":"existing broken link"}]}"##,
        )
        .expect("write matching anchor baseline");
        assert!(
            anchor_baseline_errors(&root, std::slice::from_ref(&miss))
                .expect("read matching anchor baseline")
                .is_empty()
        );
        assert!(
            anchor_baseline_errors(&root, &[])
                .expect("read stale anchor baseline")
                .iter()
                .any(|error| error.contains("stale anchor baseline"))
        );
        std::fs::remove_dir_all(root).expect("remove fixture root");
    }

    #[test]
    fn compiler_locations_map_to_lib_rs_lines_when_indented() {
        // rustc's real layout: the location line is indented under the message.
        let stderr = "error[E0425]: cannot find value `x` in this scope\n  --> src/lib.rs:12:5\n   |\n12 |     x\n\
                      warning: unused\n --> src/lib.rs:40:1\nerror: aborting\n    --> src\\lib.rs:7:2\n\
                      --> src/other.rs:3:1\n";
        assert_eq!(lib_rs_error_lines(stderr), [12, 40, 7]);
        assert!(lib_rs_error_lines("error: linking failed\n").is_empty());
    }

    #[test]
    fn extracts_inline_and_image_links() {
        let text = "[a](x.md) ![b](img.png) [c](#anchor) [d](https://example.com)";
        let targets = link_targets(text);
        assert_eq!(targets, vec!["x.md", "img.png", "#anchor", "https://example.com"]);
    }

    #[test]
    fn external_and_fragments_are_classified() {
        assert!(is_external("https://x/y"));
        assert!(!is_external("#anchor"));
        assert!(is_external("mailto:a@b"));
        assert!(is_external(""));
        assert!(!is_external("agents/scene.md"));
        assert_eq!(strip_fragment("scene.md#foo"), "scene.md");
        assert_eq!(strip_fragment("scene.md"), "scene.md");
    }

    #[test]
    fn parses_the_coverage_table() {
        let text = "\
## Agent protocol methods

| Method | MCP tool | Agent page |
|---|---|---|
| `agent.hello` | `hello` | [protocol](agents/protocol.md) |
| `scene.spawn` | `spawn` | [scene](agents/scene.md) |
";
        let (rows, errors) = coverage_table(text);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].method, "agent.hello");
        assert_eq!(rows[0].page, "agents/protocol.md");
        assert_eq!(rows[1].method, "scene.spawn");
    }

    /// integ-rules-v07b (COVERAGE parse problem): `coverage_table` reads the method table up to its
    /// first non-table line, so a blank line or a paragraph inside the table hides every row after
    /// it from `agent-coverage` and `book-check` (on integ-rules-v07a 68 dispatched methods, among
    /// them `ai.formation_get`, `server.rotation_get` and `terrain.seam_report`, read as "no row").
    /// Every method-table-shaped row of the table's own section (`` | `area.name` | `tool` |
    /// [page](agents/..) | ``) must stand inside the lines the parser reads; none may sit after a
    /// break in the table or apart in the prose.
    #[test]
    fn the_coverage_method_table_holds_every_method_row_of_its_section() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../book/COVERAGE.md");
        let text = std::fs::read_to_string(&path).expect("book/COVERAGE.md");
        let (rows, errors) = coverage_table(&text);
        assert!(errors.is_empty(), "{errors:?}");
        let lines: Vec<&str> = text.lines().collect();
        let header = lines
            .iter()
            .position(|l| {
                let cells = split_row(l);
                cells.iter().any(|c| c == "Method") && cells.iter().any(|c| c == "Agent page")
            })
            .expect("method table header");
        let start = lines[..header]
            .iter()
            .rposition(|l| l.starts_with("## "))
            .map_or(0, |i| i + 1);
        let end = lines[header..]
            .iter()
            .position(|l| l.starts_with("## "))
            .map_or(lines.len(), |i| header + i);
        let is_method_row = |line: &str| {
            let cells = split_row(line);
            cells.len() == 3
                && cells[0].len() > 2
                && cells[0].starts_with('`')
                && cells[0].ends_with('`')
                && cells[0][1..cells[0].len() - 1].contains('.')
                && cells[0][1..cells[0].len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
                && cells[1].starts_with('`')
                && cells[2].contains("](agents/")
        };
        let section: Vec<(usize, &str)> = lines[start..end]
            .iter()
            .enumerate()
            .filter(|(_, l)| is_method_row(l))
            .map(|(i, l)| (start + i + 1, *l))
            .collect();
        // The lines `coverage_table` reads: from the header to the first non-table line after a row.
        let table_end = lines[header + 1..]
            .iter()
            .position(|l| !l.trim_start().starts_with('|'))
            .map_or(lines.len(), |i| header + 1 + i);
        assert!(!rows.is_empty(), "the method table has rows");
        let outside: Vec<String> = section
            .iter()
            .filter(|(n, _)| !(header + 1..table_end).contains(&(n - 1)))
            .map(|(n, l)| format!("line {n}: {l}"))
            .collect();
        assert!(
            outside.is_empty(),
            "{} method row(s) of the method table's section stand outside the table the parser reads \
             (lines {}-{}; a blank line or prose splits the table, or a row landed in a paragraph): {outside:#?}",
            outside.len(),
            header + 1,
            table_end
        );
    }

    #[test]
    fn coverage_table_requires_the_header() {
        let (rows, errors) = coverage_table("no table here");
        assert!(rows.is_empty());
        assert!(!errors.is_empty());
    }

    #[test]
    fn every_schema_method_is_well_formed() {
        // The protocol's own invariant: method names are `<area>.<name>` and unique.
        let mut names: Vec<&str> = regia_agent::schema::METHODS.iter().map(|m| m.name).collect();
        let before = names.clone();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before.len(), "method names unique");
        for m in regia_agent::schema::METHODS {
            assert_eq!(m.name.split('.').count(), 2, "{}", m.name);
        }
    }

    #[test]
    fn fence_precheck_accepts_items_and_flags_statements() {
        let items = "use regia::prelude::*;\n\n/// Doc.\n#[derive(Component)]\nstruct A {\n    x: f32,\n}\n\nfn setup(mut commands: Commands) {\n    let a = 1;\n    commands.spawn(A { x: 1.0 });\n}\n";
        assert_eq!(first_non_item_line(items), None);
        let statement = "use regia::prelude::*;\nlet mut app = App::new();\n";
        assert_eq!(first_non_item_line(statement).map(|(l, _)| l), Some(2));
        let call = "fn f() {}\napp.add_systems(Update, f);\n";
        assert_eq!(first_non_item_line(call).map(|(l, _)| l), Some(2));
        assert_eq!(
            first_non_item_line("regia_framework::export_game!(Game);\n"),
            None
        );
        let placeholder = "fn f() {\n    ...\n}\n...\n";
        assert_eq!(first_non_item_line(placeholder).map(|(l, _)| l), Some(4));
    }

    #[test]
    fn classifies_rust_fence_info() {
        assert!(is_rust("rust"));
        assert!(is_rust("rust,no_run"));
        assert!(is_rust("rust,should_panic"));
        assert!(!is_rust("rust,ignore"));
        assert!(!is_rust("json"));
        assert!(!is_rust(""));
    }

    #[test]
    fn hidden_lines_are_compiled_but_attributes_are_not_hidden() {
        assert_eq!(unhide_line("# let x = 1;"), "let x = 1;");
        assert_eq!(unhide_line("    # let x = 1;"), "    let x = 1;");
        assert_eq!(unhide_line("#"), "");
        assert_eq!(unhide_line("#[derive(Debug)]"), "#[derive(Debug)]");
        assert_eq!(unhide_line("#![allow(dead_code)]"), "#![allow(dead_code)]");
        assert_eq!(unhide_line("## not hidden"), "# not hidden");
        assert_eq!(unhide_line("let y = 2;"), "let y = 2;");
    }

    #[test]
    fn wrap_fences_become_a_result_function() {
        assert!(is_wrapped("rust,wrap"));
        assert!(is_wrapped("rust,no_run,wrap"));
        assert!(!is_wrapped("rust"));
        assert!(!is_wrapped("rust,ignore"));
        assert!(
            !is_wrapped("rust,wrap,ignore"),
            "an ignored fence is never compiled"
        );
        let text = "```rust,wrap\n# let n = 3;\nlet m = n + 1;\nlet v: u8 = \"7\".parse()?;\n```\n";
        let blocks = fenced_rust_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].starts_with("fn book_wrapped() -> Result<(), Box<dyn std::error::Error>> {"));
        assert!(
            blocks[0].contains("\nlet n = 3;\n"),
            "the hidden line is compiled"
        );
        assert!(blocks[0].trim_end().ends_with('}'));
        assert!(
            first_non_item_line(&blocks[0]).is_none(),
            "a wrapped fence is one item"
        );
    }

    #[test]
    fn extracts_only_rust_fences() {
        let text = "\
```rust
let a = 1;
```
```json
{ \"x\": 1 }
```
```rust,no_run
let b = 2;
```
```rust,ignore
this is not compiled
```
";
        let blocks = fenced_rust_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("let a = 1;"));
        assert!(blocks[1].contains("let b = 2;"));
        assert!(!blocks[1].contains("not compiled"));
    }

    #[test]
    fn extracts_only_named_ron_fences() {
        let text = "\
```ron
(
    foo: 1,
)
```
```ron,material
( format_version: 1, shader: Standard, base: () )
```
```ron,vfx
( version: 1, name: \"x\", system: (budget: 1), emitters: [] )
```
```ron
not a named format
```
";
        let blocks = fenced_ron_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "material");
        assert!(blocks[0].1.contains("format_version"));
        assert_eq!(blocks[1].0, "vfx");
        assert!(blocks[1].1.contains("emitters"));
    }

    #[test]
    fn ron_format_tokens_map_to_loaders() {
        assert_eq!(RonFormat::from_info("material"), Some(RonFormat::Material));
        assert_eq!(RonFormat::from_info("ui"), Some(RonFormat::Ui));
        assert_eq!(RonFormat::from_info("animgraph"), Some(RonFormat::AnimGraph));
        assert_eq!(RonFormat::from_info("vfx"), Some(RonFormat::Vfx));
        assert_eq!(RonFormat::from_info("matgraph"), Some(RonFormat::MatGraph));
        assert_eq!(RonFormat::from_info("level"), None);
        assert_eq!(RonFormat::from_info(""), None);
    }

    #[test]
    fn broken_named_ron_sample_is_reported() {
        let block = RonBlock {
            file: PathBuf::from("user-guide/materials.md"),
            index: 0,
            format: RonFormat::Material,
            code: "( format_version: 99 )".to_string(),
        };
        let errors = ron_block_errors(std::slice::from_ref(&block));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("does not parse"), "{errors:?}");
        assert!(errors[0].contains("user-guide/materials.md"), "{errors:?}");
    }

    /// Each frozen-format loader must accept a minimal valid sample — this pins the loader wiring in
    /// `book-check` so a refactor of a loader cannot silently stop matching the book.
    #[test]
    fn each_loader_accepts_a_minimal_valid_sample() {
        RonFormat::Material
            .parse("(format_version: 1, shader: standard, base: ())")
            .expect("material");
        RonFormat::Ui.parse("(version: 1, root: ())").expect("ui");
        RonFormat::AnimGraph
            .parse("(format_version: 1, parameters: [], root: Clip((clip: \"X\", looping: true, speed: 1.0, sync_group: None)))")
            .expect("animgraph");
        RonFormat::Vfx
            .parse("(version: 1, name: \"x\", emitters: [(name: \"e\", spawn: (mode: Rate(1.0), lifetime: (min: 0.5, max: 1.0)), init: (size: (min: 1.0, max: 1.0)), render: (mode: Billboard))])")
            .expect("vfx");
        RonFormat::MatGraph
            .parse("(format_version: 1, nodes: { \"c\": Float(0.5) }, output: (roughness: Some(\"c\")))")
            .expect("matgraph");
        let err = RonFormat::MatGraph
            .parse("(format_version: 1, nodes: { \"c\": Float(0.5) }, output: (base_color: Some(\"c\")))")
            .expect_err("a Float base colour must not compile");
        assert!(err.contains("output:base_color"), "{err}");
    }
}
