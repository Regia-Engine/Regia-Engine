// SPDX-License-Identifier: MPL-2.0
//! Index, glossary and feature catalog for `cargo xtask manual` (ADR-10418).
//!
//! - **Index** — every `<!-- index: term, other term -->` marker in a book page, plus each page's
//!   H1 and H2 as fallback entries. `book/reference/index.md` groups them A–Z. A term links to
//!   every page that marks it, and to the heading anchor when that anchor is unique.
//! - **Glossary** — `docs/manual/glossary.ron` rendered to `book/reference/glossary.md`.
//! - **Feature catalog** — `docs/manual/features.ron` rendered to `book/reference/feature-catalog.md`,
//!   one table per area, in source order.
//!
//! Two runs on the same tree write the same bytes. The navigation pages are not scanned back into
//! the index (their letter headings would list themselves). No PDF crate is approved: the index
//! page tells the reader to print `docs/manual/manual.html`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::manual::{generated_banner, slug};
use crate::markdown::plain_text;

/// Glossary source, relative to the workspace root.
pub const GLOSSARY_RON: &str = "docs/manual/glossary.ron";

/// Feature-catalog source, relative to the workspace root.
pub const FEATURES_RON: &str = "docs/manual/features.ron";

/// Generated index, relative to `book/`.
pub const INDEX_PAGE: &str = "reference/index.md";

/// Generated glossary, relative to `book/`.
pub const GLOSSARY_PAGE: &str = "reference/glossary.md";

/// Generated feature catalog, relative to `book/`.
pub const CATALOG_PAGE: &str = "reference/feature-catalog.md";

/// Pages the index does not scan. The summary is a list of links. The navigation pages are this
/// module's output, and the inventory catalog and marker index are generated beside it. Scanning
/// those after they have been written would change the heading index on the next run.
const SKIP_PAGES: &[&str] = &[
    "SUMMARY.md",
    INDEX_PAGE,
    GLOSSARY_PAGE,
    CATALOG_PAGE,
    "manual/catalog.md",
    "manual/index.md",
];

/// One glossary entry in [`GLOSSARY_RON`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlossaryTerm {
    /// The term, as the glossary heading prints it.
    pub term: String,
    /// One or two sentences. Plain text; markup is escaped when the page is rendered.
    pub definition: String,
    /// Other terms in this file. Matched ignoring case and surrounding space.
    #[serde(default)]
    pub see_also: Vec<String>,
    /// Book path relative to `book/`, with an optional `#heading`. Empty means no page link.
    #[serde(default)]
    pub page: String,
}

/// The glossary source file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlossaryFile {
    /// Terms in any order. The page sorts them.
    pub terms: Vec<GlossaryTerm>,
}

/// One feature row in [`FEATURES_RON`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureRow {
    /// Feature name, as the table prints it.
    pub feature: String,
    /// Short status word (`shipped`, `partial`, `planned`, or another word the source uses).
    pub status: String,
    /// Book path relative to `book/`, with an optional `#heading`. Empty means no page link.
    #[serde(default)]
    pub page: String,
    /// Owning crate. Empty means the table shows an em dash.
    #[serde(default, rename = "crate")]
    pub krate: String,
    /// Specification path relative to the repository root. Empty means an em dash.
    #[serde(default)]
    pub spec: String,
}

/// One area of the feature catalog.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureArea {
    /// Area heading.
    pub area: String,
    /// Rows in the order the table prints them.
    pub rows: Vec<FeatureRow>,
}

/// The feature-catalog source file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureCatalogFile {
    /// Areas in the order the page prints them.
    pub areas: Vec<FeatureArea>,
}

/// A collected index term. Hits are keyed by `(page, anchor)` so a repeated mark is one link.
struct TermAcc {
    display: String,
    hits: BTreeMap<(String, String), String>,
}

/// One ATX heading, parsed the way `book-check` parses heading anchors.
struct RawHeading {
    level: u8,
    title: String,
    plain: String,
    raw_slug: String,
    plain_slug: String,
}

/// `book/reference/glossary.md` from the workspace's glossary source.
pub fn glossary_page(book: &Path) -> Result<String> {
    let path = workspace_root(book)?.join(GLOSSARY_RON);
    let file = load_glossary(&path)?;
    render_glossary(&file)
}

/// `book/reference/feature-catalog.md` from the workspace's catalog source.
pub fn feature_catalog_page(book: &Path) -> Result<String> {
    let root = workspace_root(book)?;
    let path = root.join(FEATURES_RON);
    let file = load_feature_catalog(&path)?;
    // Spec paths are repository files, not manual chapters, so the page names them instead of
    // linking them. A missing file fails here, where the source is read.
    for area in &file.areas {
        for row in &area.rows {
            let spec = row.spec.trim();
            if spec.is_empty() {
                continue;
            }
            let file = root.join(spec);
            if !file.is_file() {
                bail!("feature spec `{spec}` is not a file");
            }
        }
    }
    render_feature_catalog(&file)
}

/// Reads a glossary source.
pub fn load_glossary(path: &Path) -> Result<GlossaryFile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Reads a feature-catalog source.
pub fn load_feature_catalog(path: &Path) -> Result<FeatureCatalogFile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Renders the glossary. Terms sort by their folded spelling. `see_also` must name a term.
pub fn render_glossary(file: &GlossaryFile) -> Result<String> {
    let mut by_key: BTreeMap<String, &GlossaryTerm> = BTreeMap::new();
    for term in &file.terms {
        let name = term.term.trim();
        if name.is_empty() {
            bail!("glossary term is empty");
        }
        if name.chars().any(|ch| ch == '#' || ch == '\n' || ch == '\r') {
            bail!("glossary term `{name}` contains a newline or '#'");
        }
        check_book_rel(&term.page, "glossary page")?;
        let key = fold_term(name);
        if let Some(prev) = by_key.insert(key, term) {
            bail!("glossary term `{name}` repeats `{}`", prev.term.trim());
        }
    }
    for term in &file.terms {
        for other in &term.see_also {
            let key = fold_term(other);
            if key.is_empty() || !by_key.contains_key(&key) {
                bail!(
                    "glossary see-also `{}` on `{}` is not a term",
                    other.trim(),
                    term.term.trim()
                );
            }
        }
    }

    let mut slugs: BTreeMap<String, usize> = BTreeMap::new();
    for term in by_key.values() {
        let key = slug(term.term.trim());
        *slugs.entry(key).or_default() += 1;
    }
    for (key, count) in &slugs {
        if *count != 1 {
            bail!("glossary headings share the anchor `{key}` ({count} terms); rename one");
        }
    }

    let mut out = generated_banner(&format!("`{GLOSSARY_RON}`"));
    out.push_str("# Glossary\n\n");
    out.push_str(
        "The glossary defines the words the manual uses for Regia Engine. \
         Edit `docs/manual/glossary.ron` and run `cargo xtask manual`; this page is generated. \
         Terms are ordered by their spelling, ignoring case. See-also links jump to another term \
         on this page.\n\n",
    );
    if by_key.is_empty() {
        out.push_str("No terms yet.\n");
        return Ok(out);
    }

    let mut sections: BTreeMap<(u8, String), Vec<&GlossaryTerm>> = BTreeMap::new();
    for (key, term) in &by_key {
        sections.entry(section_for(key)).or_default().push(*term);
    }
    for ((_, name), terms) in &sections {
        let _ = writeln!(out, "## {name}\n");
        for term in terms {
            let display = term.term.trim();
            let _ = writeln!(out, "### {}\n", display);
            let _ = writeln!(out, "{}\n", escape_prose(term.definition.trim()));
            if !term.see_also.is_empty() {
                let links = term
                    .see_also
                    .iter()
                    .map(|name| {
                        let key = fold_term(name);
                        let canonical = by_key.get(&key).map_or(name.as_str(), |t| t.term.trim());
                        format!("[{}](#{})", link_label(canonical), slug(canonical))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "See also: {links}.\n");
            }
            if !term.page.trim().is_empty() {
                let href = book_href(GLOSSARY_PAGE, term.page.trim());
                let _ = writeln!(out, "Page: [{href}]({href}).\n");
            }
        }
    }
    Ok(out)
}

/// Renders the feature catalog. Areas and rows keep the source order.
pub fn render_feature_catalog(file: &FeatureCatalogFile) -> Result<String> {
    for area in &file.areas {
        if area.area.trim().is_empty() {
            bail!("feature catalog area name is empty");
        }
        for row in &area.rows {
            if row.feature.trim().is_empty() {
                bail!(
                    "feature catalog feature name is empty in area `{}`",
                    area.area.trim()
                );
            }
            check_book_rel(&row.page, "feature page")?;
            check_repo_rel(&row.spec, "feature spec")?;
            if row.spec.contains('`') || row.spec.contains('\n') || row.spec.contains('\r') {
                bail!(
                    "feature spec `{}` contains a backtick or a newline",
                    row.spec.trim()
                );
            }
        }
    }

    let mut out = generated_banner(&format!("`{FEATURES_RON}`"));
    out.push_str("# Feature catalog\n\n");
    out.push_str(
        "The feature catalog lists Regia Engine's features by area, with the page, crate and spec \
         for each one. Edit `docs/manual/features.ron` and run `cargo xtask manual`; this page is \
         generated. Status words in the source today are `shipped`, `partial` and `planned`; any \
         other word is printed as written. Rows stay in the source order so an inventory can append \
         them.\n\n",
    );
    if file.areas.is_empty() {
        out.push_str("No areas yet.\n");
        return Ok(out);
    }
    for area in &file.areas {
        let _ = writeln!(out, "## {}\n", area.area.trim());
        if area.rows.is_empty() {
            out.push_str("No rows yet.\n\n");
            continue;
        }
        out.push_str("| Feature | Status | Page | Crate | Spec |\n");
        out.push_str("|---|---|---|---|---|\n");
        for row in &area.rows {
            let status = if row.status.trim().is_empty() {
                "—".to_string()
            } else {
                cell(&row.status)
            };
            let page = if row.page.trim().is_empty() {
                "—".to_string()
            } else {
                let href = book_href(CATALOG_PAGE, row.page.trim());
                format!("[{}]({href})", link_label(row.feature.trim()))
            };
            let krate = if row.krate.trim().is_empty() {
                "—".to_string()
            } else {
                format!("`{}`", row.krate.trim())
            };
            // A spec lives outside the book. Naming it, rather than linking it, keeps the
            // single-file manual free of markdown links the HTML renderer cannot turn into anchors.
            let spec = if row.spec.trim().is_empty() {
                "—".to_string()
            } else {
                format!("`{}`", row.spec.trim())
            };
            let _ = writeln!(
                out,
                "| {} | {status} | {page} | {krate} | {spec} |",
                cell(row.feature.trim())
            );
        }
        out.push('\n');
    }
    Ok(out)
}

/// The index page. `overlays` replaces those book-relative paths with text that is about to be
/// written (the other generated reference pages), so the index matches this run.
pub fn render_index(book: &Path, overlays: &[(&str, &str)]) -> Result<String> {
    let mut terms: BTreeMap<String, TermAcc> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut pages = book_pages(book)?;
    pages.sort();
    for rel in pages {
        if skip_page(&rel) {
            continue;
        }
        let owned;
        let text = if let Some(over) = overlay(overlays, &rel) {
            over
        } else {
            let path = book.join(&rel);
            owned = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            owned.as_str()
        };
        seen.insert(rel.clone());
        absorb(&rel, text, &mut terms);
    }
    for (rel, text) in overlays {
        if skip_page(rel) || !seen.insert((*rel).to_string()) {
            continue;
        }
        absorb(rel, text, &mut terms);
    }
    Ok(render_terms(&terms))
}

fn absorb(path: &str, text: &str, terms: &mut BTreeMap<String, TermAcc>) {
    for (term, anchor, label) in scan_page(path, text) {
        let key = fold_term(&term);
        if key.is_empty() {
            continue;
        }
        let acc = terms.entry(key).or_insert_with(|| TermAcc {
            display: term,
            hits: BTreeMap::new(),
        });
        acc.hits.entry((path.to_string(), anchor)).or_insert(label);
    }
}

fn render_terms(terms: &BTreeMap<String, TermAcc>) -> String {
    let mut out =
        generated_banner("book pages (`<!-- index: … -->` markers and each page's H1 and H2 headings)");
    out.push_str("# Index\n\n");
    out.push_str(
        "The index lists every term marked in a book page and every H1 and H2 heading. \
         Mark a term with an HTML comment `<!-- index: term, other term -->` anywhere outside a \
         code fence. A quoted term may contain a comma. The same term may point at more than one \
         page. Within a letter, terms are ordered by their spelling, ignoring case. Each term's \
         links are ordered by page path, then by heading anchor. A heading anchor is included when \
         that heading's slug is unique on the page.\n\n",
    );
    out.push_str(
        "The single-file manual's sidebar is a table of contents: each entry jumps to that chapter. \
         To save a PDF, open `docs/manual/manual.html` in a browser and choose Print, then Save as PDF. \
         The page has a print style that shows every chapter. No PDF tool is listed in the workspace \
         dependency document, so the manual does not write a PDF file.\n\n",
    );
    if terms.is_empty() {
        out.push_str("No terms yet.\n");
        return out;
    }
    let mut sections: BTreeMap<(u8, String), Vec<&TermAcc>> = BTreeMap::new();
    for (key, acc) in terms {
        sections.entry(section_for(key)).or_default().push(acc);
    }
    for ((_, name), accs) in &sections {
        let _ = writeln!(out, "## {name}\n");
        for acc in accs {
            let links = acc
                .hits
                .iter()
                .map(|((path, anchor), label)| {
                    let href = book_href(INDEX_PAGE, &join_anchor(path, anchor));
                    format!("[{}]({href})", link_label(label))
                })
                .collect::<Vec<_>>()
                .join("; ");
            let _ = writeln!(out, "- **{}** — {links}", link_label(&acc.display));
        }
        out.push('\n');
    }
    out
}

fn join_anchor(path: &str, anchor: &str) -> String {
    if anchor.is_empty() {
        path.to_string()
    } else {
        format!("{path}#{anchor}")
    }
}

/// `(term, anchor, link label)` in page order.
fn scan_page(path: &str, text: &str) -> Vec<(String, String, String)> {
    let (placed, markers) = split_page(text);
    let mut raw_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut plain_counts: BTreeMap<String, usize> = BTreeMap::new();
    for placed_heading in &placed {
        let heading = &placed_heading.heading;
        if !heading.raw_slug.is_empty() {
            *raw_counts.entry(heading.raw_slug.clone()).or_default() += 1;
        }
        if !heading.plain_slug.is_empty() {
            *plain_counts.entry(heading.plain_slug.clone()).or_default() += 1;
        }
    }
    let anchor_of = |heading: &RawHeading| -> String {
        if heading.raw_slug.is_empty() || heading.raw_slug != heading.plain_slug {
            return String::new();
        }
        if raw_counts.get(&heading.raw_slug).copied() != Some(1) {
            return String::new();
        }
        if plain_counts.get(&heading.plain_slug).copied() != Some(1) {
            return String::new();
        }
        heading.raw_slug.clone()
    };
    let label_of = |heading: &RawHeading| -> String {
        if !heading.plain.is_empty() {
            heading.plain.clone()
        } else if !heading.title.is_empty() {
            heading.title.clone()
        } else {
            page_stem(path)
        }
    };

    let mut out = Vec::new();
    for placed_heading in &placed {
        let heading = &placed_heading.heading;
        if heading.level > 2 || heading.plain.is_empty() {
            continue;
        }
        out.push((heading.plain.clone(), anchor_of(heading), label_of(heading)));
    }
    for (line, term) in markers {
        let (anchor, label) = match heading_before(&placed, line) {
            Some(heading) => (anchor_of(heading), label_of(heading)),
            None => (String::new(), page_stem(path)),
        };
        out.push((term, anchor, label));
    }
    out
}

/// Headings carry their line number in a side table so `scan_page` can find the one before a marker.
struct PlacedHeading {
    line: usize,
    heading: RawHeading,
}

fn heading_before(headings: &[PlacedHeading], line: usize) -> Option<&RawHeading> {
    headings.iter().rev().find(|h| h.line <= line).map(|h| &h.heading)
}

fn split_page(text: &str) -> (Vec<PlacedHeading>, Vec<(usize, String)>) {
    let mut headings = Vec::new();
    let mut markers = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut pending: Option<String> = None;
    for (line_no, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let indentation = line.len() - trimmed.len();
        let marker = trimmed.chars().next();
        let marker_len = marker.map_or(0, |ch| trimmed.chars().take_while(|c| *c == ch).count());
        if let Some((fence_marker, fence_len)) = fence {
            if marker == Some(fence_marker)
                && marker_len >= fence_len
                && trimmed[marker_len..].trim().is_empty()
            {
                fence = None;
            }
            pending = None;
            continue;
        }
        if indentation <= 3
            && marker_len >= 3
            && let Some(marker @ ('`' | '~')) = marker
        {
            fence = Some((marker, marker_len));
            pending = None;
            continue;
        }
        if let Some(heading) = atx_outside_fence(line) {
            headings.push(PlacedHeading {
                line: line_no,
                heading,
            });
        }
        for body in take_comments(line, &mut pending) {
            if let Some(rest) = body.trim().strip_prefix("index:") {
                for term in split_terms(rest) {
                    markers.push((line_no, term));
                }
            }
        }
    }
    (headings, markers)
}

/// An ATX heading the way `book-check` reads one (the anchor check uses this title, not the HTML
/// renderer's).
fn atx_outside_fence(line: &str) -> Option<RawHeading> {
    let trimmed = line.trim_start();
    let indentation = line.len() - trimmed.len();
    if indentation > 3 || !trimmed.starts_with('#') {
        return None;
    }
    let rest = trimmed.trim_start_matches('#');
    let level = trimmed.len() - rest.len();
    if !(1..=6).contains(&level) || !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    if title.is_empty() {
        return None;
    }
    let Ok(level) = u8::try_from(level) else {
        return None;
    };
    let plain = plain_text(title);
    let raw_slug = slug(title);
    let plain_slug = slug(&plain);
    Some(RawHeading {
        level,
        title: title.to_string(),
        plain,
        raw_slug,
        plain_slug,
    })
}

fn take_comments(line: &str, pending: &mut Option<String>) -> Vec<String> {
    let mut closed = Vec::new();
    let mut rest = line;
    if let Some(buf) = pending.take() {
        if let Some(end) = rest.find("-->") {
            let mut full = buf;
            full.push('\n');
            full.push_str(&rest[..end]);
            closed.push(full);
            rest = &rest[end + 3..];
        } else {
            let mut full = buf;
            full.push('\n');
            full.push_str(rest);
            *pending = Some(full);
            return closed;
        }
    }
    while let Some(start) = rest.find("<!--") {
        let after = &rest[start + 4..];
        if let Some(end) = after.find("-->") {
            closed.push(after[..end].to_string());
            rest = &after[end + 3..];
        } else {
            *pending = Some(after.to_string());
            break;
        }
    }
    closed
}

/// Comma-separated terms. A double-quoted term keeps its commas. Quote marks are not part of the term.
fn split_terms(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in body.chars() {
        match ch {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                push_term(&mut out, &mut cur);
            }
            _ => cur.push(ch),
        }
    }
    push_term(&mut out, &mut cur);
    out
}

fn push_term(out: &mut Vec<String>, cur: &mut String) {
    // A marker may span lines. Collapse that whitespace so the printed term is one line.
    let term = cur.split_whitespace().collect::<Vec<_>>().join(" ");
    if !term.is_empty() {
        out.push(term);
    }
    cur.clear();
}

fn fold_term(term: &str) -> String {
    term.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// `(rank, name)` so digits sort after "Other" and before letters.
fn section_for(fold: &str) -> (u8, String) {
    for ch in fold.chars() {
        if ch.is_ascii_digit() {
            return (1, "0-9".to_string());
        }
        if ch.is_ascii_alphabetic() {
            return (2, ch.to_ascii_uppercase().to_string());
        }
        if ch.is_alphabetic() {
            return (3, ch.to_uppercase().collect());
        }
    }
    (0, "Other".to_string())
}

fn skip_page(rel: &str) -> bool {
    SKIP_PAGES.contains(&rel) || rel.split('/').any(|part| part == "build")
}

fn overlay<'a>(overlays: &'a [(&str, &str)], rel: &str) -> Option<&'a str> {
    overlays
        .iter()
        .find(|(key, _)| *key == rel)
        .map(|(_, text)| *text)
}

fn book_pages(book: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    walk_md(book, book, &mut files)?;
    Ok(files)
}

fn walk_md(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("build") {
                continue;
            }
            walk_md(root, &path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.push(rel_slash(rel));
        }
    }
    Ok(())
}

fn rel_slash(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn workspace_root(book: &Path) -> Result<&Path> {
    book.parent().context("book directory has no parent")
}

fn page_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

fn check_book_rel(page: &str, what: &str) -> Result<()> {
    let path = page.split('#').next().unwrap_or(page);
    if path.is_empty() {
        return Ok(());
    }
    if path.starts_with('/') || path.split('/').any(|part| part == ".." || part.is_empty()) {
        bail!("{what} `{page}` must be a book-relative path");
    }
    Ok(())
}

fn check_repo_rel(path: &str, what: &str) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    if path.starts_with('/') || path.split('/').any(|part| part == ".." || part.is_empty()) {
        bail!("{what} `{path}` must be a repository-relative path");
    }
    Ok(())
}

/// A link from a page under `book/` to another book-relative path (optional `#anchor` included).
fn book_href(from_page: &str, to: &str) -> String {
    let (to_path, anchor) = to.split_once('#').map_or((to, None), |(p, a)| (p, Some(a)));
    let mut href = relative_path(from_page, to_path);
    if let Some(anchor) = anchor.filter(|a| !a.is_empty()) {
        href.push('#');
        href.push_str(anchor);
    }
    href
}

fn relative_path(from_page: &str, to_page: &str) -> String {
    let from_parts: Vec<&str> = from_page.split('/').collect();
    let to_parts: Vec<&str> = to_page.split('/').filter(|p| !p.is_empty()).collect();
    let from_dir = if from_parts.len() > 1 {
        &from_parts[..from_parts.len() - 1]
    } else {
        &[]
    };
    let to_dir_len = to_parts.len().saturating_sub(1);
    let mut shared = 0;
    while shared < from_dir.len() && shared < to_dir_len && from_dir[shared] == to_parts[shared] {
        shared += 1;
    }
    let mut out = String::new();
    for _ in shared..from_dir.len() {
        out.push_str("../");
    }
    if shared < to_parts.len() {
        out.push_str(&to_parts[shared..].join("/"));
    }
    if out.is_empty() {
        to_parts.last().copied().unwrap_or("").to_string()
    } else {
        out
    }
}

fn link_label(text: &str) -> String {
    text.replace(['[', ']', '\n', '\r'], "")
}

fn escape_prose(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}
